//! Result-column provenance: map a result-grid column back to the
//! alias-qualified source column in the SELECT that produced it.
//!
//! A result column's display name is presentation-only — `SELECT p.Name AS
//! PermissionName` labels the column `PermissionName`, but a WHERE predicate
//! built from the grid must be written against `p.[Name]`: the alias does not
//! exist as a column in any table, and T-SQL does not allow referencing a
//! SELECT alias in WHERE at all. This module introspects the statement text
//! (the same statement sitting in the editor) and answers, per result column,
//! which table alias and underlying column produced it.
//!
//! Resolution is deliberately conservative: computed expressions, multi-table
//! star expansion, UNIONs, and multi-statement scripts return `None`, and the
//! caller keeps its current unqualified behavior — a wrong predicate is worse
//! than no predicate.
//!
//! Two questions, two answers. [`StatementProvenance::resolve`] answers "what
//! do I write in a predicate appended to this statement" — always a reference
//! valid in the outer scope, so a column coming out of a derived table or CTE
//! keeps that derived table's alias. [`StatementProvenance::resolve_origin`]
//! answers "where did this value ultimately come from" for display: it traces
//! one level through a derived table or CTE body to the base table and column
//! (deeper nesting stays unresolved).

/// The source expression a result column came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceColumn {
    /// Qualifier parts before the column: a table alias (`["p"]`), or the
    /// schema-qualified table chain (`["Shared", "Permission"]`) when the
    /// query has no alias. Empty means unqualified.
    pub qualifier: Vec<String>,
    /// The underlying column name — never the SELECT-list alias.
    pub column: String,
}

/// The base table and column a result column ultimately came from, traced one
/// level through a derived table or CTE when needed. For display (tooltips,
/// status surfaces) — not necessarily a reference that is valid in the outer
/// statement's scope; predicates must use [`SourceColumn`] instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnOrigin {
    /// The base table's identifier chain, e.g. `["Shared", "Permission"]`.
    pub table: Vec<String>,
    /// The alias the base table carries in its own scope, when it has one.
    pub alias: Option<String>,
    /// The base table's column name.
    pub column: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    /// Identifier or keyword, dequoted ([x], "x", `x` all yield `x`).
    Ident(String),
    /// Single punctuation/operator character (only `.` `,` `(` `)` `*` `;`
    /// matter to this parser).
    Punct(char),
    /// A number, string literal, or anything else opaque.
    Other,
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && bytes.get(i + 1) == Some(&'-') {
            while i < bytes.len() && bytes[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && bytes.get(i + 1) == Some(&'*') {
            i += 2;
            while i < bytes.len() && !(bytes[i] == '*' && bytes.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else if c == '\'' {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == '\'' {
                    if bytes.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                i += 1;
            }
            out.push(Tok::Other);
        } else if c == '[' {
            let mut name = String::new();
            i += 1;
            while i < bytes.len() {
                if bytes[i] == ']' {
                    if bytes.get(i + 1) == Some(&']') {
                        name.push(']');
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                name.push(bytes[i]);
                i += 1;
            }
            out.push(Tok::Ident(name));
        } else if c == '"' || c == '`' {
            let quote = c;
            let mut name = String::new();
            i += 1;
            while i < bytes.len() {
                if bytes[i] == quote {
                    if bytes.get(i + 1) == Some(&quote) {
                        name.push(quote);
                        i += 2;
                        continue;
                    }
                    i += 1;
                    break;
                }
                name.push(bytes[i]);
                i += 1;
            }
            out.push(Tok::Ident(name));
        } else if c.is_alphabetic() || c == '_' || c == '@' || c == '#' {
            let mut name = String::new();
            while i < bytes.len()
                && (bytes[i].is_alphanumeric() || matches!(bytes[i], '_' | '@' | '#' | '$'))
            {
                name.push(bytes[i]);
                i += 1;
            }
            out.push(Tok::Ident(name));
        } else if c.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == '.') {
                i += 1;
            }
            out.push(Tok::Other);
        } else {
            out.push(Tok::Punct(c));
            i += 1;
        }
    }
    out
}

fn is_kw(tok: &Tok, kw: &str) -> bool {
    matches!(tok, Tok::Ident(s) if s.eq_ignore_ascii_case(kw))
}

fn is_any_kw(tok: &Tok, kws: &[&str]) -> bool {
    kws.iter().any(|k| is_kw(tok, k))
}

/// One SELECT-list item, analyzed.
#[derive(Debug, Clone)]
struct Item {
    /// The name this item projects, when statically known: the explicit
    /// alias, or the last identifier of a plain column chain.
    output: Option<String>,
    /// `(qualifier_chain, column)` when the expression is a plain (possibly
    /// qualified) column reference. `None` for computed expressions.
    source: Option<(Vec<String>, String)>,
    /// `Some(qualifier_chain)` for `*` (empty chain) or `alias.*`.
    star: Option<Vec<String>>,
}

/// A FROM-clause table reference.
#[derive(Debug, Clone)]
struct TableRef {
    /// Identifier chain of the table (`["Shared", "Permission"]`); empty for
    /// a derived table (subquery).
    chain: Vec<String>,
    alias: Option<String>,
    /// Column aliases of a derived table: `(...) d (c1, c2)`. Empty when the
    /// derived table exposes its inner select-list names.
    column_aliases: Vec<String>,
    /// The parsed body of a derived table, when it is a single plain SELECT.
    /// Parsed one level deep only — a derived table inside a derived table
    /// stays opaque.
    inner: Option<Box<InnerSelect>>,
}

impl TableRef {
    /// How to qualify a column of this table: the alias when present,
    /// otherwise the (schema-qualified) table chain itself.
    fn qualifier(&self) -> Vec<String> {
        match &self.alias {
            Some(a) => vec![a.clone()],
            None => self.chain.clone(),
        }
    }
}

/// The select-list items and FROM tables of a parsed inner SELECT (a derived
/// table or CTE body).
#[derive(Debug, Clone)]
struct InnerSelect {
    items: Vec<Item>,
    tables: Vec<TableRef>,
}

/// One `WITH name [(cols)] AS (...)` common table expression.
#[derive(Debug, Clone)]
struct Cte {
    name: String,
    /// The explicit column list, when present — it renames the body's
    /// projections positionally.
    column_aliases: Vec<String>,
    /// The parsed body, when it is a single plain SELECT (a recursive or
    /// UNION body stays opaque).
    inner: Option<InnerSelect>,
}

const CLAUSE_STARTERS: &[&str] = &[
    "FROM",
    "WHERE",
    "GROUP",
    "ORDER",
    "HAVING",
    "UNION",
    "EXCEPT",
    "INTERSECT",
    "INTO",
    "OPTION",
    "LIMIT",
    "OFFSET",
    "FETCH",
    "FOR",
    "WINDOW",
    "QUALIFY",
    "GO",
];

const JOIN_WORDS: &[&str] = &["JOIN", "INNER", "LEFT", "RIGHT", "FULL", "CROSS", "OUTER"];

/// Parses the `WITH` prologue at the start of `tokens`, returning the CTEs
/// and the index where the statement proper begins. Anything that does not
/// parse cleanly yields no CTEs and offset 0, so the statement falls back to
/// treating CTE references as opaque tables (today's behavior).
fn parse_ctes(tokens: &[Tok]) -> (Vec<Cte>, usize) {
    if !tokens.first().is_some_and(|t| is_kw(t, "WITH")) {
        return (Vec::new(), 0);
    }
    let mut i = 1;
    let mut ctes = Vec::new();
    loop {
        let Some(Tok::Ident(name)) = tokens.get(i) else {
            return (Vec::new(), 0);
        };
        let name = name.clone();
        i += 1;
        let mut column_aliases = Vec::new();
        if tokens.get(i) == Some(&Tok::Punct('(')) {
            let Some((aliases, next)) = ident_list(tokens, i) else {
                return (Vec::new(), 0);
            };
            column_aliases = aliases;
            i = next;
        }
        if !tokens.get(i).is_some_and(|t| is_kw(t, "AS")) {
            return (Vec::new(), 0);
        }
        i += 1;
        if tokens.get(i) != Some(&Tok::Punct('(')) {
            return (Vec::new(), 0);
        }
        let body_start = i + 1;
        let mut d = 0i32;
        while i < tokens.len() {
            match tokens[i] {
                Tok::Punct('(') => d += 1,
                Tok::Punct(')') => {
                    d -= 1;
                    if d == 0 {
                        break;
                    }
                }
                _ => {}
            }
            i += 1;
        }
        if i >= tokens.len() {
            return (Vec::new(), 0);
        }
        let inner = parse_select_tokens(&tokens[body_start..i], 1)
            .map(|(items, tables)| InnerSelect { items, tables });
        i += 1; // past the closing ')'
        ctes.push(Cte {
            name,
            column_aliases,
            inner,
        });
        if tokens.get(i) == Some(&Tok::Punct(',')) {
            i += 1;
            continue;
        }
        break;
    }
    (ctes, i)
}

/// Parses `( ident, ident, ... )` starting at the `(` at `tokens[i]`,
/// returning the identifiers and the index just past the closing `)`.
fn ident_list(tokens: &[Tok], i: usize) -> Option<(Vec<String>, usize)> {
    debug_assert_eq!(tokens.get(i), Some(&Tok::Punct('(')));
    let mut idents = Vec::new();
    let mut j = i + 1;
    loop {
        match tokens.get(j) {
            Some(Tok::Ident(name)) => {
                idents.push(name.clone());
                j += 1;
            }
            _ => return None,
        }
        match tokens.get(j) {
            Some(Tok::Punct(',')) => j += 1,
            Some(Tok::Punct(')')) => return Some((idents, j + 1)),
            _ => return None,
        }
    }
}

/// Parses the (single) top-level SELECT of `tokens` into its select-list
/// items and FROM-clause table references. Returns `None` when the script has
/// no top-level SELECT, or more than one (multiple statements, UNION) — there
/// is no safe way to attribute a result column then. `level` 0 is the outer
/// statement; derived-table bodies parse at `level` 1 and stop descending.
fn parse_select_tokens(tokens: &[Tok], level: u8) -> Option<(Vec<Item>, Vec<TableRef>)> {
    // Locate depth-0 SELECTs.
    let mut depth = 0i32;
    let mut select_positions = Vec::new();
    for (i, tok) in tokens.iter().enumerate() {
        match tok {
            Tok::Punct('(') => depth += 1,
            Tok::Punct(')') => depth -= 1,
            t if depth == 0 && is_kw(t, "SELECT") => select_positions.push(i),
            _ => {}
        }
    }
    if select_positions.len() != 1 {
        return None;
    }
    let mut i = select_positions[0] + 1;

    // Skip DISTINCT / ALL / TOP n [PERCENT] [WITH TIES].
    while i < tokens.len() {
        if is_any_kw(&tokens[i], &["DISTINCT", "ALL"]) {
            i += 1;
        } else if is_kw(&tokens[i], "TOP") {
            i += 1;
            if tokens.get(i) == Some(&Tok::Punct('(')) {
                let mut d = 0;
                while i < tokens.len() {
                    match tokens[i] {
                        Tok::Punct('(') => d += 1,
                        Tok::Punct(')') => {
                            d -= 1;
                            if d == 0 {
                                i += 1;
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            } else {
                i += 1; // the count
            }
            if tokens.get(i).is_some_and(|t| is_kw(t, "PERCENT")) {
                i += 1;
            }
            if tokens.get(i).is_some_and(|t| is_kw(t, "WITH"))
                && tokens.get(i + 1).is_some_and(|t| is_kw(t, "TIES"))
            {
                i += 2;
            }
        } else {
            break;
        }
    }

    // Collect select-list items (split on depth-0 commas).
    let mut items = Vec::new();
    let mut current: Vec<Tok> = Vec::new();
    let mut depth = 0i32;
    while i < tokens.len() {
        let tok = &tokens[i];
        if depth == 0 && matches!(tok, Tok::Ident(_)) && is_any_kw(tok, CLAUSE_STARTERS) {
            break;
        }
        if depth == 0 && *tok == Tok::Punct(';') {
            break;
        }
        match tok {
            Tok::Punct('(') => {
                depth += 1;
                current.push(tok.clone());
            }
            Tok::Punct(')') => {
                depth -= 1;
                current.push(tok.clone());
            }
            Tok::Punct(',') if depth == 0 => {
                items.push(analyze_item(&current));
                current.clear();
            }
            _ => current.push(tok.clone()),
        }
        i += 1;
    }
    if !current.is_empty() {
        items.push(analyze_item(&current));
    }
    if items.is_empty() {
        return None;
    }

    // Parse the FROM clause, if present.
    let mut tables = Vec::new();
    if i < tokens.len() && is_kw(&tokens[i], "FROM") {
        i += 1;
        tables = parse_from(tokens, &mut i, level);
    }
    Some((items, tables))
}

fn parse_from(tokens: &[Tok], i: &mut usize, level: u8) -> Vec<TableRef> {
    let mut tables = Vec::new();
    let terminators: &[&str] = &[
        "WHERE",
        "GROUP",
        "ORDER",
        "HAVING",
        "UNION",
        "EXCEPT",
        "INTERSECT",
        "OPTION",
        "LIMIT",
        "OFFSET",
        "FETCH",
        "FOR",
        "WINDOW",
        "QUALIFY",
        "GO",
    ];
    loop {
        // One table reference: chain or (subquery), then optional alias.
        let mut chain: Vec<String> = Vec::new();
        let mut inner: Option<Box<InnerSelect>> = None;
        if tokens.get(*i) == Some(&Tok::Punct('(')) {
            // Derived table: capture the balanced parens; at the outer level,
            // parse the body one level deep.
            let body_start = *i + 1;
            let mut d = 0;
            while *i < tokens.len() {
                match tokens[*i] {
                    Tok::Punct('(') => d += 1,
                    Tok::Punct(')') => {
                        d -= 1;
                        if d == 0 {
                            break;
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
            if level == 0 && *i <= tokens.len() {
                inner = parse_select_tokens(&tokens[body_start..(*i).min(tokens.len())], 1)
                    .map(|(items, tables)| Box::new(InnerSelect { items, tables }));
            }
            if *i < tokens.len() {
                *i += 1; // past the closing ')'
            }
        } else {
            while let Some(Tok::Ident(name)) = tokens.get(*i) {
                if is_any_kw(&tokens[*i], terminators) || is_any_kw(&tokens[*i], JOIN_WORDS) {
                    break;
                }
                chain.push(name.clone());
                *i += 1;
                if tokens.get(*i) == Some(&Tok::Punct('.')) {
                    *i += 1;
                } else {
                    break;
                }
            }
            if chain.is_empty() {
                break;
            }
        }
        // Optional AS / alias (an identifier that is not a keyword).
        let mut alias = None;
        if tokens.get(*i).is_some_and(|t| is_kw(t, "AS")) {
            *i += 1;
        }
        if let Some(Tok::Ident(name)) = tokens.get(*i)
            && !is_any_kw(&tokens[*i], terminators)
            && !is_any_kw(&tokens[*i], JOIN_WORDS)
            && !is_any_kw(&tokens[*i], &["ON", "WITH", "USING"])
        {
            alias = Some(name.clone());
            *i += 1;
        }
        // A derived table's paren list of plain identifiers is its column
        // alias list: `(...) d (c1, c2)`.
        let mut column_aliases = Vec::new();
        if chain.is_empty()
            && tokens.get(*i) == Some(&Tok::Punct('('))
            && let Some((aliases, next)) = ident_list(tokens, *i)
        {
            column_aliases = aliases;
            *i = next;
        }
        // Table hints: WITH ( ... ) or a bare ( ... ) after the name.
        if tokens.get(*i).is_some_and(|t| is_kw(t, "WITH")) {
            *i += 1;
        }
        if tokens.get(*i) == Some(&Tok::Punct('(')) {
            let mut d = 0;
            while *i < tokens.len() {
                match tokens[*i] {
                    Tok::Punct('(') => d += 1,
                    Tok::Punct(')') => {
                        d -= 1;
                        if d == 0 {
                            *i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
                *i += 1;
            }
        }
        tables.push(TableRef {
            chain,
            alias,
            column_aliases,
            inner,
        });

        // Advance to the next reference: skip ON conditions, stop at clause
        // terminators, continue on comma or JOIN words.
        let mut depth = 0i32;
        let mut next_ref = false;
        while *i < tokens.len() {
            let tok = &tokens[*i];
            match tok {
                Tok::Punct('(') => depth += 1,
                Tok::Punct(')') => depth -= 1,
                Tok::Punct(',') if depth == 0 => {
                    *i += 1;
                    next_ref = true;
                    break;
                }
                Tok::Punct(';') if depth == 0 => return tables,
                t if depth == 0 && is_any_kw(t, terminators) => return tables,
                t if depth == 0 && is_any_kw(t, JOIN_WORDS) => {
                    // Skip the JOIN word run (INNER JOIN, LEFT OUTER JOIN …).
                    while tokens.get(*i).is_some_and(|t| is_any_kw(t, JOIN_WORDS)) {
                        *i += 1;
                    }
                    next_ref = true;
                    break;
                }
                _ => {}
            }
            *i += 1;
        }
        if !next_ref {
            break;
        }
    }
    tables
}

/// Analyzes one select-list item's tokens.
fn analyze_item(tokens: &[Tok]) -> Item {
    // `*` or `chain.*`
    if tokens.last() == Some(&Tok::Punct('*')) {
        if tokens.len() == 1 {
            return Item {
                output: None,
                source: None,
                star: Some(Vec::new()),
            };
        }
        // chain '.' '*'
        if tokens.get(tokens.len() - 2) == Some(&Tok::Punct('.'))
            && let Some(chain) = ident_chain(&tokens[..tokens.len() - 2])
        {
            return Item {
                output: None,
                source: None,
                star: Some(chain),
            };
        }
        // `expr * something` (multiplication) — computed.
        return Item {
            output: None,
            source: None,
            star: None,
        };
    }

    // Explicit alias: `expr AS name`.
    if tokens.len() >= 3
        && is_kw(&tokens[tokens.len() - 2], "AS")
        && let Some(Tok::Ident(alias)) = tokens.last()
    {
        let expr = &tokens[..tokens.len() - 2];
        return Item {
            output: Some(alias.clone()),
            source: ident_chain(expr).map(split_chain),
            star: None,
        };
    }

    // Whole item is a plain column chain.
    if let Some(chain) = ident_chain(tokens) {
        let (qualifier, column) = split_chain(chain);
        return Item {
            output: Some(column.clone()),
            source: Some((qualifier, column)),
            star: None,
        };
    }

    // Implicit alias: `expr name` where the last token is an identifier not
    // joined by a dot (e.g. `COUNT(*) cnt`, `p.Name PermissionName`).
    if tokens.len() >= 2
        && !matches!(tokens[tokens.len() - 2], Tok::Punct('.'))
        && let Some(Tok::Ident(alias)) = tokens.last()
    {
        let expr = &tokens[..tokens.len() - 1];
        return Item {
            output: Some(alias.clone()),
            source: ident_chain(expr).map(split_chain),
            star: None,
        };
    }

    Item {
        output: None,
        source: None,
        star: None,
    }
}

/// `a`, `a.b`, `a.b.c` → the identifier chain; anything else → `None`.
fn ident_chain(tokens: &[Tok]) -> Option<Vec<String>> {
    if tokens.is_empty() {
        return None;
    }
    let mut chain = Vec::new();
    for (idx, tok) in tokens.iter().enumerate() {
        if idx % 2 == 0 {
            match tok {
                Tok::Ident(name) => chain.push(name.clone()),
                _ => return None,
            }
        } else if *tok != Tok::Punct('.') {
            return None;
        }
    }
    if tokens.len().is_multiple_of(2) {
        return None; // ends on a dot
    }
    Some(chain)
}

/// Splits an identifier chain into `(qualifier_parts, column)`.
fn split_chain(mut chain: Vec<String>) -> (Vec<String>, String) {
    let column = chain.pop().unwrap_or_default();
    (chain, column)
}

/// One parsed SELECT statement, ready to answer provenance questions for any
/// number of result columns. Parse once per statement (see
/// [`StatementProvenance::parse`]) instead of re-lexing the SQL per column.
pub struct StatementProvenance {
    items: Vec<Item>,
    tables: Vec<TableRef>,
    ctes: Vec<Cte>,
    has_star: bool,
}

impl StatementProvenance {
    /// Parses the single top-level SELECT of `sql`. `None` when there is no
    /// safe statement to attribute against (no SELECT, UNION, multiple
    /// statements).
    pub fn parse(sql: &str) -> Option<Self> {
        let tokens = tokenize(sql);
        let (ctes, rest) = parse_ctes(&tokens);
        let (items, tables) = parse_select_tokens(&tokens[rest..], 0)?;
        let has_star = items.iter().any(|item| item.star.is_some());
        Some(Self {
            items,
            tables,
            ctes,
            has_star,
        })
    }

    /// Resolves the source column behind result column `label`, clicked at
    /// result position `column_index`. `label_occurrences` is how many result
    /// columns share this label — pass the count from the actual result set
    /// so star expansion cannot be confused with an identically named
    /// explicit projection.
    ///
    /// Returns `None` whenever attribution is not certain — computed
    /// expressions, multi-table or ambiguous star expansion — so callers fall
    /// back to their current behavior.
    pub fn resolve(
        &self,
        label: &str,
        column_index: usize,
        label_occurrences: usize,
    ) -> Option<SourceColumn> {
        // With a star in the list, result positions no longer line up with
        // select-list items, and a duplicated label may be the explicit item
        // *or* a same-named column the star expanded — no way to tell which
        // one was clicked without catalog data. Only a label that appears
        // exactly once in the result set is attributable.
        if self.has_star && label_occurrences != 1 {
            return None;
        }

        // The single-table qualifier, when the FROM clause has exactly one
        // table.
        let single_table_qualifier = || -> Option<Vec<String>> {
            match self.tables.as_slice() {
                [only] => {
                    let q = only.qualifier();
                    (!q.is_empty()).then_some(q)
                }
                _ => None,
            }
        };

        // Prefer the item that projects `label`; on duplicates, use the
        // clicked position to break the tie (positions align when the list
        // has no star).
        let named: Vec<&Item> = self
            .items
            .iter()
            .filter(|item| {
                item.output
                    .as_deref()
                    .is_some_and(|o| o.eq_ignore_ascii_case(label))
            })
            .collect();
        let matched: Option<&Item> = match named.as_slice() {
            [one] => Some(one),
            [] => None,
            _ if !self.has_star => self.items.get(column_index).filter(|item| {
                item.output
                    .as_deref()
                    .is_some_and(|o| o.eq_ignore_ascii_case(label))
            }),
            _ => None,
        };

        if let Some(item) = matched {
            let (qualifier, column) = item.source.clone()?;
            let qualifier = if qualifier.is_empty() {
                single_table_qualifier().unwrap_or_default()
            } else {
                qualifier
            };
            return Some(SourceColumn { qualifier, column });
        }

        // Not an explicit item: attribute through a star, when unambiguous.
        let stars: Vec<&Vec<String>> = self
            .items
            .iter()
            .filter_map(|item| item.star.as_ref())
            .collect();
        if let [star_qualifier] = stars.as_slice() {
            let qualifier = if star_qualifier.is_empty() {
                single_table_qualifier()?
            } else {
                (*star_qualifier).clone()
            };
            return Some(SourceColumn {
                qualifier,
                column: label.to_string(),
            });
        }
        None
    }

    /// Resolves the base table and column behind result column `label`,
    /// tracing one level through a derived table or CTE body when the value
    /// comes out of one. Parameters as in [`Self::resolve`]. `None` whenever
    /// the origin is not certain — including anything nested deeper than one
    /// level.
    pub fn resolve_origin(
        &self,
        label: &str,
        column_index: usize,
        label_occurrences: usize,
    ) -> Option<ColumnOrigin> {
        let source = self.resolve(label, column_index, label_occurrences)?;
        self.origin_of(&source)
    }

    /// Maps an outer-scope [`SourceColumn`] to the base table it refers to.
    fn origin_of(&self, source: &SourceColumn) -> Option<ColumnOrigin> {
        match source.qualifier.as_slice() {
            [] => None,
            [q] => {
                let table = self.tables.iter().find(|t| table_answers_to(t, q))?;
                self.base_of_table_column(table, &source.column, true)
            }
            chain => Some(ColumnOrigin {
                table: chain.to_vec(),
                alias: None,
                column: source.column.clone(),
            }),
        }
    }

    /// The origin of `column` of FROM-reference `table`. `descend` allows one
    /// level of tracing into a derived table or CTE body; the inner scope
    /// passes `false`, so anything deeper stays unresolved.
    fn base_of_table_column(
        &self,
        table: &TableRef,
        column: &str,
        descend: bool,
    ) -> Option<ColumnOrigin> {
        if table.chain.is_empty() {
            if !descend {
                return None;
            }
            let inner = table.inner.as_deref()?;
            return self.origin_through_inner(inner, &table.column_aliases, column);
        }
        if let [name] = table.chain.as_slice()
            && let Some(cte) = self.ctes.iter().find(|c| c.name.eq_ignore_ascii_case(name))
        {
            if !descend {
                return None;
            }
            let inner = cte.inner.as_ref()?;
            return self.origin_through_inner(inner, &cte.column_aliases, column);
        }
        Some(ColumnOrigin {
            table: table.chain.clone(),
            alias: table.alias.clone(),
            column: column.to_string(),
        })
    }

    /// Traces the outer-visible column `label` through a derived table or CTE
    /// body to its base column.
    fn origin_through_inner(
        &self,
        inner: &InnerSelect,
        column_aliases: &[String],
        label: &str,
    ) -> Option<ColumnOrigin> {
        let item: &Item = if column_aliases.is_empty() {
            let named: Vec<&Item> = inner
                .items
                .iter()
                .filter(|item| {
                    item.output
                        .as_deref()
                        .is_some_and(|o| o.eq_ignore_ascii_case(label))
                })
                .collect();
            match named.as_slice() {
                [one] => one,
                // Not an explicit projection: attribute through the body's
                // star, when unambiguous.
                [] => return self.origin_via_inner_star(inner, label),
                _ => return None,
            }
        } else {
            // An explicit column list renames the projections positionally.
            let pos = column_aliases
                .iter()
                .position(|a| a.eq_ignore_ascii_case(label))?;
            inner.items.get(pos)?
        };
        let (qualifier, column) = item.source.clone()?;
        match qualifier.as_slice() {
            [] => match inner.tables.as_slice() {
                [only] => self.base_of_table_column(only, &column, false),
                _ => None,
            },
            [q] => {
                let table = inner.tables.iter().find(|t| table_answers_to(t, q))?;
                self.base_of_table_column(table, &column, false)
            }
            chain => Some(ColumnOrigin {
                table: chain.to_vec(),
                alias: None,
                column,
            }),
        }
    }

    /// Attributes `label` through the single star of an inner select body.
    fn origin_via_inner_star(&self, inner: &InnerSelect, label: &str) -> Option<ColumnOrigin> {
        let stars: Vec<&Vec<String>> = inner
            .items
            .iter()
            .filter_map(|item| item.star.as_ref())
            .collect();
        let [star_qualifier] = stars.as_slice() else {
            return None;
        };
        let table = match star_qualifier.as_slice() {
            [] => match inner.tables.as_slice() {
                [only] => only,
                _ => return None,
            },
            [q] => inner.tables.iter().find(|t| table_answers_to(t, q))?,
            chain => inner
                .tables
                .iter()
                .find(|t| chains_equal(&t.chain, chain))?,
        };
        self.base_of_table_column(table, label, false)
    }
}

/// Whether FROM-reference `table` is what a single-part qualifier `q` names:
/// its alias, or — for an unaliased table — its (last) name part.
fn table_answers_to(table: &TableRef, q: &str) -> bool {
    match &table.alias {
        Some(alias) => alias.eq_ignore_ascii_case(q),
        None => table
            .chain
            .last()
            .is_some_and(|last| last.eq_ignore_ascii_case(q)),
    }
}

fn chains_equal(a: &[String], b: &[String]) -> bool {
    a.len() == b.len() && a.iter().zip(b).all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// One-shot convenience over [`StatementProvenance`] for callers resolving a
/// single column. `label_occurrences` as in [`StatementProvenance::resolve`].
pub fn resolve_source_column(
    sql: &str,
    label: &str,
    column_index: usize,
    label_occurrences: usize,
) -> Option<SourceColumn> {
    StatementProvenance::parse(sql)?.resolve(label, column_index, label_occurrences)
}

/// One-shot convenience over [`StatementProvenance::resolve_origin`].
pub fn resolve_column_origin(
    sql: &str,
    label: &str,
    column_index: usize,
    label_occurrences: usize,
) -> Option<ColumnOrigin> {
    StatementProvenance::parse(sql)?.resolve_origin(label, column_index, label_occurrences)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(sql: &str, label: &str, index: usize) -> Option<(Vec<&'static str>, &'static str)> {
        // Leak for terse assertions in tests only.
        resolve_source_column(sql, label, index, 1).map(|s| {
            (
                s.qualifier
                    .into_iter()
                    .map(|q| &*Box::leak(q.into_boxed_str()))
                    .collect(),
                &*Box::leak(s.column.into_boxed_str()),
            )
        })
    }

    /// The reported user query: SELECT-list aliases must resolve to the
    /// alias-qualified source columns.
    #[test]
    fn resolves_aliases_of_the_reported_query() {
        let sql = "SELECT\n    rm.Name as ModelName,\n    p.Name as PermissionName\nFROM Shared.ReferenceModelPermissions rmp\n    INNER JOIN Shared.Permission p ON p.PermissionId = rmp.PermissionId\n    INNER JOIN Shared.ReferenceModel rm ON rm.ReferenceModelId = rmp.ReferenceModelId";
        assert_eq!(resolve(sql, "PermissionName", 1), Some((vec!["p"], "Name")));
        assert_eq!(resolve(sql, "ModelName", 0), Some((vec!["rm"], "Name")));
    }

    #[test]
    fn implicit_alias_resolves_like_as() {
        let sql = "SELECT p.Name PermissionName FROM Shared.Permission p";
        assert_eq!(resolve(sql, "PermissionName", 0), Some((vec!["p"], "Name")));
    }

    #[test]
    fn unaliased_qualified_column_keeps_its_qualifier() {
        let sql = "SELECT p.Name FROM Shared.Permission p JOIN Other.T t ON t.Id = p.Id";
        assert_eq!(resolve(sql, "Name", 0), Some((vec!["p"], "Name")));
    }

    /// A bare column over a single table gains that table's alias; with no
    /// alias the schema-qualified table name qualifies it.
    #[test]
    fn bare_column_qualifies_from_a_single_table() {
        assert_eq!(
            resolve("SELECT Name FROM Shared.Permission p", "Name", 0),
            Some((vec!["p"], "Name"))
        );
        assert_eq!(
            resolve("SELECT Name FROM Shared.Permission", "Name", 0),
            Some((vec!["Shared", "Permission"], "Name"))
        );
    }

    /// A bare column over a join is ambiguous without catalog data: it keeps
    /// the column name but gains no qualifier.
    #[test]
    fn bare_column_over_a_join_stays_unqualified() {
        let sql =
            "SELECT Name FROM Shared.Permission p JOIN Shared.ReferenceModel rm ON rm.Id = p.Id";
        assert_eq!(resolve(sql, "Name", 0), Some((vec![], "Name")));
    }

    #[test]
    fn star_over_a_single_table_attributes_to_it() {
        assert_eq!(
            resolve("SELECT * FROM Shared.Permission p", "PermissionId", 3),
            Some((vec!["p"], "PermissionId"))
        );
        assert_eq!(
            resolve("SELECT * FROM Shared.Permission", "PermissionId", 3),
            Some((vec!["Shared", "Permission"], "PermissionId"))
        );
    }

    #[test]
    fn qualified_star_attributes_to_its_alias() {
        let sql = "SELECT p.* FROM Shared.Permission p JOIN Other.T t ON t.Id = p.Id";
        assert_eq!(resolve(sql, "Name", 2), Some((vec!["p"], "Name")));
    }

    #[test]
    fn star_over_a_join_is_not_attributed() {
        let sql = "SELECT * FROM A a JOIN B b ON b.Id = a.Id";
        assert_eq!(resolve(sql, "Name", 0), None);
    }

    /// Computed and derived expressions have no single source column.
    #[test]
    fn computed_expressions_do_not_resolve() {
        assert_eq!(resolve("SELECT COUNT(*) AS Total FROM T", "Total", 0), None);
        assert_eq!(
            resolve(
                "SELECT p.First + p.Last AS FullName FROM People p",
                "FullName",
                0
            ),
            None
        );
    }

    #[test]
    fn unions_and_multiple_statements_do_not_resolve() {
        assert_eq!(
            resolve("SELECT a FROM T UNION SELECT a FROM U", "a", 0),
            None
        );
        assert_eq!(resolve("SELECT a FROM T; SELECT b FROM U", "a", 0), None);
    }

    /// Duplicate output names use the clicked position to pick the item.
    #[test]
    fn duplicate_labels_resolve_by_position() {
        let sql = "SELECT p.Name, rm.Name FROM P p JOIN RM rm ON rm.Id = p.Id";
        assert_eq!(resolve(sql, "Name", 0), Some((vec!["p"], "Name")));
        assert_eq!(resolve(sql, "Name", 1), Some((vec!["rm"], "Name")));
    }

    /// Quoted identifiers dequote consistently across dialect quote styles.
    #[test]
    fn quoted_identifiers_dequote() {
        assert_eq!(
            resolve(
                "SELECT p.[Name] AS [Permission Name] FROM [Shared].[Permission] p",
                "Permission Name",
                0
            ),
            Some((vec!["p"], "Name"))
        );
        assert_eq!(
            resolve(
                "SELECT p.\"Name\" AS \"PermissionName\" FROM \"Shared\".\"Permission\" p",
                "PermissionName",
                0
            ),
            Some((vec!["p"], "Name"))
        );
        assert_eq!(
            resolve(
                "SELECT `p`.`Name` AS `PermissionName` FROM `Shared`.`Permission` `p`",
                "PermissionName",
                0
            ),
            Some((vec!["p"], "Name"))
        );
    }

    #[test]
    fn subselect_in_the_list_does_not_confuse_the_split() {
        let sql = "SELECT p.Name AS N, (SELECT COUNT(*) FROM X WHERE X.Id = p.Id) AS C FROM P p";
        assert_eq!(resolve(sql, "N", 0), Some((vec!["p"], "Name")));
        assert_eq!(resolve(sql, "C", 1), None);
    }

    #[test]
    fn top_and_distinct_are_skipped() {
        assert_eq!(
            resolve("SELECT TOP 100 p.Name AS N FROM P p", "N", 0),
            Some((vec!["p"], "Name"))
        );
        assert_eq!(
            resolve("SELECT DISTINCT p.Name AS N FROM P p", "N", 0),
            Some((vec!["p"], "Name"))
        );
    }

    /// A CTE's inner SELECT sits inside parentheses, so the outer statement
    /// is still the single top-level SELECT and resolves normally — the CTE
    /// name qualifies like any other table reference.
    #[test]
    fn cte_outer_select_resolves() {
        let sql = "WITH x AS (SELECT a, b FROM T) SELECT x.a AS Alpha FROM x";
        assert_eq!(resolve(sql, "Alpha", 0), Some((vec!["x"], "a")));
        // Bare column over the single CTE reference qualifies with it too.
        let sql = "WITH x AS (SELECT a FROM T) SELECT a FROM x";
        assert_eq!(resolve(sql, "a", 0), Some((vec!["x"], "a")));
    }

    /// Mixed `*` and explicit projections: when the clicked label appears
    /// more than once in the result set, the explicit item and a same-named
    /// star-expanded column cannot be told apart — no attribution.
    #[test]
    fn mixed_star_and_duplicate_label_is_not_attributed() {
        let sql = "SELECT p.Name AS X, t.* FROM P p JOIN T t ON t.Id = p.Id";
        // Two result columns labeled X (the alias and t's own X column).
        assert_eq!(resolve_source_column(sql, "X", 0, 2), None);
        assert_eq!(resolve_source_column(sql, "X", 3, 2), None);
        // Unique label: safe — it can only be the explicit projection.
        assert_eq!(
            resolve_source_column(sql, "X", 0, 1).map(|s| (s.qualifier, s.column)),
            Some((vec!["p".to_string()], "Name".to_string()))
        );
    }

    /// Quoted identifiers may contain parentheses or start with a digit —
    /// the quote branch consumes them whole, so neither confuses the parser.
    #[test]
    fn quoted_identifiers_with_odd_content_parse() {
        assert_eq!(
            resolve(
                "SELECT p.[col(with paren)] AS C FROM [Shared].[Permission] p",
                "C",
                0
            ),
            Some((vec!["p"], "col(with paren)"))
        );
        assert_eq!(
            resolve(
                "SELECT u.\"5_users\" AS Five FROM \"5_users_table\" u",
                "Five",
                0
            ),
            Some((vec!["u"], "5_users"))
        );
    }

    fn origin(
        sql: &str,
        label: &str,
        index: usize,
    ) -> Option<(Vec<String>, Option<String>, String)> {
        resolve_column_origin(sql, label, index, 1).map(|o| (o.table, o.alias, o.column))
    }

    fn owned(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|p| p.to_string()).collect()
    }

    /// One level of derived table: predicates keep the derived alias, while
    /// the origin traces to the base table behind it.
    #[test]
    fn derived_table_resolves_one_level() {
        let sql = "SELECT sub.y AS Z FROM (SELECT x AS y FROM T) sub";
        assert_eq!(resolve(sql, "Z", 0), Some((vec!["sub"], "y")));
        assert_eq!(
            origin(sql, "Z", 0),
            Some((owned(&["T"]), None, "x".to_string()))
        );
    }

    #[test]
    fn star_over_a_derived_table_traces_to_the_base_column() {
        let sql = "SELECT * FROM (SELECT p.Name AS y FROM Shared.Permission p) sub";
        assert_eq!(resolve(sql, "y", 0), Some((vec!["sub"], "y")));
        assert_eq!(
            origin(sql, "y", 0),
            Some((
                owned(&["Shared", "Permission"]),
                Some("p".to_string()),
                "Name".to_string()
            ))
        );
    }

    /// A star inside the derived table attributes through it when the body
    /// has a single base table.
    #[test]
    fn star_inside_a_derived_table_attributes_to_its_single_table() {
        let sql = "SELECT sub.Name AS N FROM (SELECT * FROM Shared.Permission p) sub";
        assert_eq!(
            origin(sql, "N", 0),
            Some((
                owned(&["Shared", "Permission"]),
                Some("p".to_string()),
                "Name".to_string()
            ))
        );
    }

    /// Derived-table column aliases rename the projections positionally.
    #[test]
    fn derived_table_column_aliases_map_positionally() {
        let sql = "SELECT d.b AS Out FROM (SELECT p.Name, p.Id FROM P p) d (a, b)";
        assert_eq!(
            origin(sql, "Out", 0),
            Some((owned(&["P"]), Some("p".to_string()), "Id".to_string()))
        );
    }

    /// One level of CTE: the outer reference keeps the CTE name for
    /// predicates, and the origin traces through the CTE body.
    #[test]
    fn cte_resolves_one_level() {
        let sql =
            "WITH x AS (SELECT p.Name AS a FROM Shared.Permission p) SELECT x.a AS Alpha FROM x";
        assert_eq!(resolve(sql, "Alpha", 0), Some((vec!["x"], "a")));
        assert_eq!(
            origin(sql, "Alpha", 0),
            Some((
                owned(&["Shared", "Permission"]),
                Some("p".to_string()),
                "Name".to_string()
            ))
        );
    }

    #[test]
    fn cte_column_list_renames_positionally() {
        let sql = "WITH x (u, v) AS (SELECT p.Name, p.Id FROM P p) SELECT v FROM x";
        assert_eq!(resolve(sql, "v", 0), Some((vec!["x"], "v")));
        assert_eq!(
            origin(sql, "v", 0),
            Some((owned(&["P"]), Some("p".to_string()), "Id".to_string()))
        );
    }

    #[test]
    fn multiple_ctes_resolve_independently() {
        let sql = "WITH a AS (SELECT Id FROM T), b AS (SELECT u.Name FROM U u) \
                   SELECT a.Id AS I, b.Name AS N FROM a JOIN b ON b.Id = a.Id";
        assert_eq!(
            origin(sql, "I", 0),
            Some((owned(&["T"]), None, "Id".to_string()))
        );
        assert_eq!(
            origin(sql, "N", 1),
            Some((owned(&["U"]), Some("u".to_string()), "Name".to_string()))
        );
    }

    /// Depth stops at one level: a derived table inside a derived table, or a
    /// CTE referencing another CTE, stays unresolved.
    #[test]
    fn nesting_deeper_than_one_level_stays_unresolved() {
        let sql = "SELECT o.y AS Z FROM (SELECT i.y FROM (SELECT x AS y FROM T) i) o";
        assert_eq!(origin(sql, "Z", 0), None);
        let sql = "WITH a AS (SELECT x AS y FROM T), b AS (SELECT y FROM a) SELECT b.y AS Z FROM b";
        assert_eq!(origin(sql, "Z", 0), None);
        // A derived table whose body reads from a CTE is two levels too.
        let sql = "WITH a AS (SELECT x AS y FROM T) SELECT d.y AS Z FROM (SELECT y FROM a) d";
        assert_eq!(origin(sql, "Z", 0), None);
    }

    /// A recursive / UNION CTE body is opaque; the outer reference still
    /// resolves for predicates.
    #[test]
    fn union_cte_bodies_stay_opaque() {
        let sql = "WITH x AS (SELECT a FROM T UNION ALL SELECT a FROM U) SELECT x.a AS A FROM x";
        assert_eq!(resolve(sql, "A", 0), Some((vec!["x"], "a")));
        assert_eq!(origin(sql, "A", 0), None);
    }

    /// A computed inner projection has no single origin, but a plain base
    /// table origin still reports directly.
    #[test]
    fn origin_of_a_plain_table_column_is_the_table_itself() {
        let sql = "SELECT p.Name AS PermissionName FROM Shared.Permission p";
        assert_eq!(
            origin(sql, "PermissionName", 0),
            Some((
                owned(&["Shared", "Permission"]),
                Some("p".to_string()),
                "Name".to_string()
            ))
        );
        let sql = "SELECT d.c AS C FROM (SELECT COUNT(*) AS c FROM T) d";
        assert_eq!(origin(sql, "C", 0), None);
    }

    /// Ambiguity inside the body defeats attribution: two inner items with
    /// the same output, or a bare inner column over a join.
    #[test]
    fn ambiguous_inner_bodies_do_not_attribute() {
        let sql = "SELECT d.n AS N FROM (SELECT a.n, b.n FROM A a JOIN B b ON b.i = a.i) d";
        assert_eq!(origin(sql, "N", 0), None);
        let sql = "SELECT d.n AS N FROM (SELECT n FROM A a JOIN B b ON b.i = a.i) d";
        assert_eq!(origin(sql, "N", 0), None);
    }

    #[test]
    fn table_hints_do_not_become_aliases() {
        assert_eq!(
            resolve(
                "SELECT Name FROM Shared.Permission WITH (NOLOCK)",
                "Name",
                0
            ),
            Some((vec!["Shared", "Permission"], "Name"))
        );
    }
}
