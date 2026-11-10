//! # seclex — the security lexer
//!
//! The one authoritative model of where each database engine's string
//! literals, quoted identifiers and comments begin and end. Every
//! security-relevant decision (the SELECT-only gate, the read-only screen,
//! rollback-wrap refusals, mutation review, tenant/destructive checks, the
//! statement pieces a driver sends) reads SQL through this module, so no two
//! gates can disagree about what is code.
//!
//! ## Why readings, not a lexer
//!
//! The same bytes mean different things on different engines — `` `'` `` is
//! a quoted identifier on MySQL and the start of a string literal on SQL
//! Server — and even on one engine they can depend on server state the client
//! cannot see: `sql_mode` (`NO_BACKSLASH_ESCAPES`, `ANSI_QUOTES`),
//! `standard_conforming_strings`, the server version that decides whether a
//! `/*!50003 … */` comment runs, the ClickHouse release. A gate that picks
//! one interpretation can be walked around by text that the server reads the
//! other way (`SELECT 1 AS `'`; DELETE FROM t; SELECT 1 AS `'`` hides the
//! DELETE from a lexer that does not know backtick identifiers).
//!
//! So [`views`] returns one [`View`] per *reading* of the text that could move
//! a token boundary — the engine's default reading first — and the caller
//! judges every one: a gate passes only when all readings pass. Each view is
//! a *shadow* of the text (same byte length, spans index both) in which only
//! live code survives; see [`scan`] for the exact contract. The shadow is fed
//! to the dialect-invariant tokenizer, whose keyword table the safety
//! classifier already relies on.
//!
//! ## Fail closed
//!
//! - An unterminated literal, identifier, comment or dollar quote has its
//!   opener blanked and everything after it lexed again as code, so nothing
//!   can hide inside one.
//! - [`Views::ambiguity`] reports what the gates must refuse outright: a NUL
//!   byte (engines disagree about where input ends), an unterminated
//!   construct under the engine's default reading, or more readings than the
//!   bound (pathological executable-comment version soup).
//! - Engines with no modelled grammar (no native driver) are judged under
//!   every modelled engine at once ([`LexTarget::AnyEngine`]).
//!
//! ## Execution units
//!
//! Engines that take one statement per request (DuckDB, libSQL, ClickHouse,
//! Oracle) and SQL Server's `GO` batches are split by SQLLY before they reach
//! the server. [`execution_units`] is that split — the drivers send exactly
//! these units, and the gates judge exactly these units.

use crate::sqlmodel::{ConnectionKind, DatabaseType, Dialect};

mod rules;
mod scan;
mod units;

pub use scan::{Construct, ConstructKind, Unclosed, Unterminated};
pub use units::{ExecUnit, execution_units, split_top_level, strip_leading_trivia};

/// A lexical grammar SQLLY models exactly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LexEngine {
    /// SQL Server / Azure SQL (T-SQL).
    TSql,
    /// PostgreSQL and the servers that run its lexer (Yugabyte, the cloud
    /// PostgreSQL services).
    Postgres,
    /// CockroachDB: PostgreSQL syntax, standard strings always on, `b''`
    /// byte strings with escapes.
    CockroachDb,
    /// DuckDB: PostgreSQL-derived lexer, standard strings always on.
    DuckDb,
    /// MySQL, MariaDB, TiDB, PlanetScale and the hosted MySQL services.
    MySql,
    ClickHouse,
    /// SQLite and libSQL.
    Sqlite,
    Oracle,
}

impl LexEngine {
    pub const ALL: [LexEngine; 8] = [
        LexEngine::TSql,
        LexEngine::Postgres,
        LexEngine::CockroachDb,
        LexEngine::DuckDb,
        LexEngine::MySql,
        LexEngine::ClickHouse,
        LexEngine::Sqlite,
        LexEngine::Oracle,
    ];

    /// The grammar a connection's engine speaks, or `None` when SQLLY has no
    /// driver for it (then [`LexTarget::AnyEngine`] applies) or it is not SQL.
    pub fn for_database(db: DatabaseType) -> Option<LexEngine> {
        match db.connection_kind() {
            ConnectionKind::Tds => Some(LexEngine::TSql),
            ConnectionKind::PostgresWire if db == DatabaseType::CockroachDb => {
                Some(LexEngine::CockroachDb)
            }
            ConnectionKind::PostgresWire => Some(LexEngine::Postgres),
            ConnectionKind::MySqlWire => Some(LexEngine::MySql),
            ConnectionKind::SqliteFile | ConnectionKind::HranaHttp => Some(LexEngine::Sqlite),
            ConnectionKind::DuckDbFile => Some(LexEngine::DuckDb),
            ConnectionKind::ClickHouseHttp => Some(LexEngine::ClickHouse),
            ConnectionKind::OracleNet => Some(LexEngine::Oracle),
            _ => None,
        }
    }

    /// The editor lexer's grammar family for a dialect.
    pub fn for_dialect(dialect: Dialect) -> LexEngine {
        match dialect {
            Dialect::SqlServer => LexEngine::TSql,
            Dialect::Postgres => LexEngine::Postgres,
            Dialect::MySql => LexEngine::MySql,
            Dialect::Oracle => LexEngine::Oracle,
            Dialect::Sqlite => LexEngine::Sqlite,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            LexEngine::TSql => "SQL Server",
            LexEngine::Postgres => "PostgreSQL",
            LexEngine::CockroachDb => "CockroachDB",
            LexEngine::DuckDb => "DuckDB",
            LexEngine::MySql => "MySQL-family",
            LexEngine::ClickHouse => "ClickHouse",
            LexEngine::Sqlite => "SQLite",
            LexEngine::Oracle => "Oracle",
        }
    }

    /// Whether statements run without a `;` between them (T-SQL batches), so
    /// a statement verb anywhere starts a new statement.
    pub fn separators_optional(self) -> bool {
        self == LexEngine::TSql
    }
}

/// Which grammar(s) to read text under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexTarget {
    Engine(LexEngine),
    /// Every modelled engine at once: for text whose destination is unknown
    /// or has no modelled grammar. A verdict must hold under all of them.
    AnyEngine,
}

impl LexTarget {
    pub fn for_database(db: DatabaseType) -> LexTarget {
        LexEngine::for_database(db)
            .map(LexTarget::Engine)
            .unwrap_or(LexTarget::AnyEngine)
    }
}

/// One reading of the text.
#[derive(Debug, Clone)]
pub struct View {
    pub engine: LexEngine,
    /// Plain-English name of the reading, for refusal messages.
    pub reading: String,
    /// The engine's default-configuration reading.
    pub primary: bool,
    /// Same byte length as the input; only live code survives.
    pub shadow: String,
    /// The first construct this reading found unterminated.
    pub unterminated: Option<Unterminated>,
}

impl View {
    /// The dialect-invariant token stream of the shadow: spans index the
    /// original text; string literals read as `'…'` with blank contents and
    /// quoted identifiers as `[…]` identifiers.
    pub fn tokens(&self) -> Vec<crate::tokenizer::Token<'_>> {
        crate::tokenizer::tokenize(&self.shadow)
    }
}

/// Every reading of a text.
#[derive(Debug, Clone)]
pub struct Views {
    pub views: Vec<View>,
    /// Byte offset of the first NUL, if any.
    pub nul_at: Option<usize>,
    /// More readings than [`rules::MAX_READINGS`] for some engine.
    pub overflowed: bool,
}

impl Views {
    /// Why no reading of the text can be trusted, or `None`. Gates must
    /// refuse (or demand confirmation for) an ambiguous text.
    ///
    /// An unterminated construct counts when every engine in the target
    /// reads one under its default settings: for one engine that is simply
    /// "the default reading is broken"; for [`LexTarget::AnyEngine`] a text
    /// that is valid somewhere (a T-SQL `'C:\dir\'` is unterminated only
    /// to a MySQL reading) is not refused for it. Either way nothing hides in
    /// an unterminated construct — its tail is lexed again as code.
    pub fn ambiguity(&self) -> Option<String> {
        if let Some(at) = self.nul_at {
            return Some(format!(
                "the text contains a NUL character (byte {at}); engines disagree about where such input ends"
            ));
        }
        if self.overflowed {
            return Some(
                "the text has too many version-gated executable comments to judge every way the server could read them"
                    .to_string(),
            );
        }
        let mut first: Option<(&View, Unterminated)> = None;
        for view in self.views.iter().filter(|v| v.primary) {
            let u = view.unterminated?;
            first.get_or_insert((view, u));
        }
        first.map(|(v, u)| {
            format!(
                "{} reads an unterminated {} starting at byte {}",
                v.engine.label(),
                u.what.describe(),
                u.offset
            )
        })
    }

    /// The primary reading of `engine` (or the first view).
    pub fn primary(&self) -> &View {
        self.views
            .iter()
            .find(|v| v.primary)
            .unwrap_or(&self.views[0])
    }
}

/// Every reading of `sql` under `target` that could move a token boundary,
/// each engine's default reading first. Identical shadows are merged.
pub fn views(sql: &str, target: LexTarget) -> Views {
    let engines: &[LexEngine] = match target {
        LexTarget::Engine(ref e) => std::slice::from_ref(e),
        LexTarget::AnyEngine => &LexEngine::ALL,
    };
    use std::hash::{Hash, Hasher};
    let digest = |text: &str| {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        hasher.finish()
    };
    let mut out: Vec<View> = Vec::new();
    let mut seen: Vec<(LexEngine, u64, usize)> = Vec::new();
    let mut overflowed = false;
    for &engine in engines {
        let (readings, over) = rules::readings(sql, engine);
        overflowed |= over;
        for reading in readings {
            let scanned = scan::scan(sql, &reading.rules);
            let hash = digest(&scanned.shadow);
            if !reading.primary
                && seen.iter().any(|&(e, h, index)| {
                    e == engine && h == hash && out[index].shadow == scanned.shadow
                })
            {
                continue;
            }
            seen.push((engine, hash, out.len()));
            out.push(View {
                engine,
                reading: reading.label,
                primary: reading.primary,
                shadow: scanned.shadow,
                unterminated: scanned.unterminated,
            });
        }
    }
    Views {
        views: out,
        nul_at: sql.bytes().position(|b| b == 0),
        overflowed,
    }
}

/// Convenience: [`views`] for a connection's engine.
pub fn views_for(sql: &str, db: DatabaseType) -> Views {
    views(sql, LexTarget::for_database(db))
}

/// The construct the editor's dialect lexer must treat as one token at
/// `pos` (a token start), under `dialect`'s default reading with executable
/// comments left as comments. This is how `sqllex::StrictLexer` shares the
/// boundary rules instead of keeping its own.
pub fn editor_construct_at(bytes: &[u8], pos: usize, dialect: Dialect) -> Option<Construct> {
    let rules = rules::editor(LexEngine::for_dialect(dialect));
    scan::construct_at(bytes, pos, &rules)
}

/// The reading the editor's dialect lexer — and so the parser, validation
/// and the tenant-scope check — applies to `sql` for `dialect`.
pub fn editor_view(sql: &str, dialect: Dialect) -> View {
    let engine = LexEngine::for_dialect(dialect);
    let scanned = scan::scan(sql, &rules::editor(engine));
    View {
        engine,
        reading: format!("{} (editor)", engine.label()),
        primary: true,
        shadow: scanned.shadow,
        unterminated: scanned.unterminated,
    }
}

/// Whether `b` continues a bare word under `dialect`'s default reading.
pub fn editor_word_continue(b: u8, dialect: Dialect) -> bool {
    rules::editor(LexEngine::for_dialect(dialect)).is_word_continue(b)
}

#[cfg(test)]
mod tests;
