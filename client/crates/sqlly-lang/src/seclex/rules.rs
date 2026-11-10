//! One fully-resolved lexical reading of an engine: every rule that decides
//! where a string literal, quoted identifier, or comment begins and ends.
//!
//! An engine usually has several possible readings of the same text because
//! part of its lexical grammar depends on server state the client cannot see
//! (`sql_mode`, `standard_conforming_strings`, the server version that gates a
//! MySQL executable comment, the ClickHouse release). [`readings`] enumerates
//! every reading that can change a token boundary in *this* text; the caller
//! judges each one and fails closed unless all of them pass.

use super::LexEngine;

/// How a `"…"` construct is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum DoubleQuote {
    /// A quoted identifier. `doubling`: `""` stands for one `"` inside
    /// (false for Oracle, whose quoted identifiers cannot contain `"`).
    Ident { backslash: bool, doubling: bool },
    /// A string literal (MySQL without `ANSI_QUOTES`).
    Str { backslash: bool },
}

/// How a `[…]` construct is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Bracket {
    /// Not a quote at all (a bare `[` is array syntax or an error).
    None,
    /// T-SQL: `]]` stands for one `]` inside.
    Doubled,
    /// SQLite: MS-Access style, ends at the first `]` with no escape.
    NoEscape,
}

/// When `#` starts a line comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum HashComment {
    Never,
    /// MySQL / MariaDB: always.
    Always,
    /// ClickHouse: only `# ` and `#!`.
    SpaceOrBang,
}

/// Which `$…$` quoting the engine has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Dollar {
    None,
    /// PostgreSQL family: `$$…$$` / `$tag$…$tag$`, tag an identifier that
    /// does not start with a digit and contains no `$`.
    Tagged,
    /// ClickHouse heredoc: `$tag$…$tag$`, tag `[A-Za-z0-9_]*` (verified
    /// live on 26.9: `$1$…$1$` and `$$…$$` quote, `$é$` and `$a b$` do not).
    Heredoc,
}

/// Which MySQL-compatible server reads the executable comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ExecFlavor {
    /// Treat every `/*!…*/` as an ordinary comment (the editor's lexer).
    Inert,
    /// MySQL: `/*!` runs (versioned: when `version <= server`); `/*M!` and
    /// `/*T!` are ordinary comments.
    MySql,
    /// MariaDB: `/*!` and `/*M!` run by version, except that `/*!` versions in
    /// MySQL's 5.7+ range (50700–99999) are ignored.
    MariaDb,
    /// TiDB: `/*!` as MySQL, plus `/*T![feature] … */` runs.
    TiDb,
}

/// Executable-comment reading for the MySQL family.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Exec {
    pub flavor: ExecFlavor,
    /// MySQL 8.0.34+ accepts six-digit versions; older servers read five.
    pub six_digit_versions: bool,
    /// The server version the reading assumes.
    pub server_version: u64,
}

/// The decision for one `/*!`-style comment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExecDecision {
    /// The payload is live SQL; `marker_len` bytes (`/*!` + version digits)
    /// are the marker to blank.
    Run { marker_len: usize },
    /// An ordinary comment. `nest_allowance` is how many nested `/*` levels
    /// the server tolerates inside (a version-gated comment that does not run
    /// may hold one; a plain comment none).
    Comment { nest_allowance: u32 },
}

/// A fully-resolved lexical reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Rules {
    pub engine: LexEngine,
    /// Backslash escapes inside `'…'`.
    pub single_backslash: bool,
    pub double: DoubleQuote,
    /// `` `…` `` quoted identifiers; the flag is "backslash escapes inside".
    pub backtick: Option<bool>,
    pub bracket: Bracket,
    pub nested_comments: bool,
    pub hash: HashComment,
    /// MySQL: `--` starts a comment only when followed by whitespace, a
    /// control character, or the end of input.
    pub dash_needs_space: bool,
    /// Line comments also end at a bare `\r`.
    pub line_ends_at_cr: bool,
    /// PostgreSQL `E'…'` (backslash escapes).
    pub e_strings: bool,
    /// CockroachDB `b'…'` byte strings (backslash escapes).
    pub b_escapes: bool,
    pub dollar: Dollar,
    /// Oracle `q'X…X'` alternative quoting (and `nq'…'`).
    pub oracle_q: bool,
    /// SQLite `$`/`@`/`:`/`#` variables, including the TCL `$a(…)` form that
    /// may contain quotes.
    pub sqlite_vars: bool,
    /// SQLite: an unterminated `/*` is a valid comment running to the end.
    pub open_block_comment_ok: bool,
    pub exec: Option<Exec>,
}

impl Rules {
    pub(crate) fn exec_decision(&self, body: &[u8]) -> ExecDecision {
        // `body` starts right after `/*`.
        let Some(exec) = self.exec else {
            return ExecDecision::Comment { nest_allowance: 0 };
        };
        let (marker, marker_len) = match body {
            [b'!', ..] => (b'!', 1usize),
            [b'M' | b'm', b'!', ..] => (b'M', 2),
            [b'T' | b't', b'!', ..] => (b'T', 2),
            _ => return ExecDecision::Comment { nest_allowance: 0 },
        };
        let after = &body[marker_len..];
        let digits = after.iter().take_while(|b| b.is_ascii_digit()).count();
        let plain = ExecDecision::Comment { nest_allowance: 0 };
        match exec.flavor {
            ExecFlavor::Inert => plain,
            ExecFlavor::MySql | ExecFlavor::TiDb => {
                if marker == b'M' {
                    return plain;
                }
                if marker == b'T' {
                    return if exec.flavor == ExecFlavor::TiDb {
                        ExecDecision::Run {
                            marker_len: 2 + marker_len,
                        }
                    } else {
                        plain
                    };
                }
                if digits < 5 {
                    return ExecDecision::Run {
                        marker_len: 2 + marker_len,
                    };
                }
                let len = if exec.six_digit_versions && digits >= 6 {
                    6
                } else {
                    5
                };
                let version = parse_digits(&after[..len]);
                if version <= exec.server_version {
                    ExecDecision::Run {
                        marker_len: 2 + marker_len + len,
                    }
                } else {
                    ExecDecision::Comment { nest_allowance: 1 }
                }
            }
            ExecFlavor::MariaDb => {
                if marker == b'T' {
                    return plain;
                }
                if digits < 5 {
                    return ExecDecision::Run {
                        marker_len: 2 + marker_len,
                    };
                }
                let len = if digits >= 6 { 6 } else { 5 };
                let version = parse_digits(&after[..len]);
                let maria = marker == b'M';
                if version <= exec.server_version
                    && (!(50_700..=99_999).contains(&version) || maria)
                {
                    ExecDecision::Run {
                        marker_len: 2 + marker_len + len,
                    }
                } else {
                    ExecDecision::Comment { nest_allowance: 1 }
                }
            }
        }
    }

    /// Bytes that continue a bare word. Where a word ends decides where a
    /// prefixed string (`E'…'`, `q'…'`) or a dollar quote may begin.
    pub(crate) fn is_word_continue(&self, b: u8) -> bool {
        if b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80 {
            return true;
        }
        match self.engine {
            LexEngine::TSql => matches!(b, b'@' | b'#' | b'$'),
            LexEngine::Oracle => matches!(b, b'$' | b'#'),
            // ClickHouse identifiers may contain `$` (`b$x$a` is one name).
            _ => b == b'$',
        }
    }

    pub(crate) fn is_word_start(&self, b: u8) -> bool {
        if b.is_ascii_alphabetic() || b == b'_' || b >= 0x80 {
            return true;
        }
        self.engine == LexEngine::TSql && matches!(b, b'@' | b'#')
    }
}

fn parse_digits(digits: &[u8]) -> u64 {
    digits
        .iter()
        .fold(0u64, |acc, d| acc * 10 + u64::from(d - b'0'))
}

/// The engine's reading under its default server configuration.
pub(crate) fn primary(engine: LexEngine) -> Rules {
    let base = Rules {
        engine,
        single_backslash: false,
        double: DoubleQuote::Ident {
            backslash: false,
            doubling: true,
        },
        backtick: None,
        bracket: Bracket::None,
        nested_comments: false,
        hash: HashComment::Never,
        dash_needs_space: false,
        line_ends_at_cr: false,
        e_strings: false,
        b_escapes: false,
        dollar: Dollar::None,
        oracle_q: false,
        sqlite_vars: false,
        open_block_comment_ok: false,
        exec: None,
    };
    match engine {
        LexEngine::TSql => Rules {
            bracket: Bracket::Doubled,
            nested_comments: true,
            // Verified live (SQL Server 2025): a bare `\r` ends a `--` comment.
            line_ends_at_cr: true,
            ..base
        },
        LexEngine::Postgres | LexEngine::CockroachDb | LexEngine::DuckDb => Rules {
            nested_comments: true,
            // PostgreSQL's `--` comment is `[^\n\r]*`.
            line_ends_at_cr: engine != LexEngine::CockroachDb,
            e_strings: true,
            b_escapes: engine == LexEngine::CockroachDb,
            dollar: Dollar::Tagged,
            ..base
        },
        LexEngine::MySql => Rules {
            single_backslash: true,
            double: DoubleQuote::Str { backslash: true },
            backtick: Some(false),
            hash: HashComment::Always,
            dash_needs_space: true,
            exec: Some(Exec {
                flavor: ExecFlavor::MySql,
                six_digit_versions: false,
                server_version: 80_099,
            }),
            ..base
        },
        LexEngine::ClickHouse => Rules {
            single_backslash: true,
            double: DoubleQuote::Ident {
                backslash: true,
                doubling: true,
            },
            backtick: Some(true),
            nested_comments: true,
            hash: HashComment::SpaceOrBang,
            dollar: Dollar::Heredoc,
            ..base
        },
        LexEngine::Sqlite => Rules {
            backtick: Some(false),
            bracket: Bracket::NoEscape,
            sqlite_vars: true,
            open_block_comment_ok: true,
            ..base
        },
        LexEngine::Oracle => Rules {
            double: DoubleQuote::Ident {
                backslash: false,
                doubling: false,
            },
            oracle_q: true,
            ..base
        },
    }
}

/// The reading the editor's dialect lexer (`sqllex::StrictLexer`) uses:
/// the primary reading, with executable comments left as comments (the parser
/// and highlighter treat `/*!…*/` as trivia; safety exposes them separately).
pub(crate) fn editor(engine: LexEngine) -> Rules {
    let mut rules = primary(engine);
    if let Some(exec) = rules.exec.as_mut() {
        exec.flavor = ExecFlavor::Inert;
    }
    rules
}

/// A reading plus a plain-English label for refusal messages.
#[derive(Debug, Clone)]
pub(crate) struct Reading {
    pub rules: Rules,
    pub label: String,
    pub primary: bool,
}

/// Upper bound on readings per engine; past it the text is judged ambiguous.
pub(crate) const MAX_READINGS: usize = 96;

/// Every reading of `sql` that can move a token boundary, primary first.
/// Dimensions are only expanded when the text contains the byte that makes
/// them matter, so ordinary SQL has exactly one reading per engine.
/// Returns `(readings, overflowed)`.
pub(crate) fn readings(sql: &str, engine: LexEngine) -> (Vec<Reading>, bool) {
    let bytes = sql.as_bytes();
    let has_backslash = bytes.contains(&b'\\');
    let has_double_quote = bytes.contains(&b'"');
    let has_lone_cr = bytes
        .iter()
        .enumerate()
        .any(|(i, &b)| b == b'\r' && bytes.get(i + 1) != Some(&b'\n'));
    let base = primary(engine);
    let mut out = vec![Reading {
        rules: base,
        label: format!("{} (default settings)", engine.label()),
        primary: true,
    }];
    let mut push = |rules: Rules, label: String| {
        if !out.iter().any(|r| r.rules == rules) {
            out.push(Reading {
                rules,
                label,
                primary: false,
            });
        }
    };

    match engine {
        LexEngine::TSql | LexEngine::Oracle | LexEngine::CockroachDb => {
            if has_lone_cr {
                push(
                    Rules {
                        line_ends_at_cr: !base.line_ends_at_cr,
                        ..base
                    },
                    format!(
                        "{} (bare carriage return ends a -- comment)",
                        engine.label()
                    ),
                );
            }
        }
        LexEngine::Postgres => {
            if has_backslash {
                push(
                    Rules {
                        single_backslash: true,
                        ..base
                    },
                    "PostgreSQL (standard_conforming_strings = off)".to_string(),
                );
            }
        }
        LexEngine::DuckDb => {
            if bytes
                .windows(2)
                .any(|w| matches!(w[0], b'e' | b'E') && w[1] == b'\'')
            {
                push(
                    Rules {
                        e_strings: false,
                        ..base
                    },
                    "DuckDB (E'' read as a plain string)".to_string(),
                );
            }
        }
        LexEngine::ClickHouse => {
            let comment_openers = bytes.windows(2).filter(|w| w == b"/*").count();
            let has_dollar = bytes.contains(&b'$');
            let nestings: &[bool] = if comment_openers >= 2 {
                &[true, false]
            } else {
                &[true]
            };
            let heredocs: &[Dollar] = if has_dollar {
                &[Dollar::Heredoc, Dollar::None]
            } else {
                &[Dollar::Heredoc]
            };
            for &nested in nestings {
                for &dollar in heredocs {
                    push(
                        Rules {
                            nested_comments: nested,
                            dollar,
                            ..base
                        },
                        format!(
                            "ClickHouse ({} block comments, heredoc {})",
                            if nested { "nested" } else { "flat" },
                            if dollar == Dollar::Heredoc {
                                "on"
                            } else {
                                "off"
                            }
                        ),
                    );
                }
            }
        }
        LexEngine::MySql => {
            let backslash: &[bool] = if has_backslash {
                &[true, false]
            } else {
                &[true]
            };
            let ansi: &[bool] = if has_double_quote {
                &[false, true]
            } else {
                &[false]
            };
            let execs = exec_readings(bytes);
            for &bs in backslash {
                for &ansi_quotes in ansi {
                    for exec in &execs {
                        let double = if ansi_quotes {
                            DoubleQuote::Ident {
                                backslash: false,
                                doubling: true,
                            }
                        } else {
                            DoubleQuote::Str { backslash: bs }
                        };
                        let mut label = String::from("MySQL-family (");
                        label.push_str(if bs {
                            "backslash escapes"
                        } else {
                            "NO_BACKSLASH_ESCAPES"
                        });
                        if ansi_quotes {
                            label.push_str(", ANSI_QUOTES");
                        }
                        label.push_str(&format!(
                            ", {} {}",
                            match exec.flavor {
                                ExecFlavor::Inert | ExecFlavor::MySql => "MySQL",
                                ExecFlavor::MariaDb => "MariaDB",
                                ExecFlavor::TiDb => "TiDB",
                            },
                            exec.server_version
                        ));
                        label.push(')');
                        push(
                            Rules {
                                single_backslash: bs,
                                double,
                                exec: Some(*exec),
                                ..base
                            },
                            label,
                        );
                    }
                }
            }
        }
        LexEngine::Sqlite => {}
    }
    let overflowed = out.len() > MAX_READINGS;
    out.truncate(MAX_READINGS);
    (out, overflowed)
}

/// Executable-comment readings worth distinguishing for this text: every
/// server flavor at every version threshold that the text's own version
/// numbers can separate (plus "nothing versioned runs" and "everything
/// runs"). The real server is always one of them.
fn exec_readings(bytes: &[u8]) -> Vec<Exec> {
    let base = primary(LexEngine::MySql)
        .exec
        .expect("mysql has exec comments");
    let mut versions5: Vec<u64> = Vec::new();
    let mut versions6: Vec<u64> = Vec::new();
    let mut any_marker = false;
    let mut i = 0;
    while i + 2 < bytes.len() {
        if bytes[i] == b'/' && bytes[i + 1] == b'*' {
            let rest = &bytes[i + 2..];
            let after = match rest {
                [b'!', tail @ ..] => Some(tail),
                [b'M' | b'm' | b'T' | b't', b'!', tail @ ..] => Some(tail),
                _ => None,
            };
            if let Some(tail) = after {
                any_marker = true;
                let digits = tail.iter().take_while(|b| b.is_ascii_digit()).count();
                if digits >= 5 {
                    versions5.push(parse_digits(&tail[..5]));
                }
                if digits >= 6 {
                    versions6.push(parse_digits(&tail[..6]));
                }
            }
        }
        i += 1;
    }
    if !any_marker {
        return vec![base];
    }
    let thresholds = |versions: &[u64]| {
        let mut t: Vec<u64> = vec![0, u64::MAX];
        for &v in versions {
            t.push(v);
            t.push(v.saturating_sub(1));
        }
        t.sort_unstable();
        t.dedup();
        t
    };
    let all: Vec<u64> = versions5.iter().chain(&versions6).copied().collect();
    let mut out = vec![base];
    let mut push = |exec: Exec| {
        if !out.contains(&exec) {
            out.push(exec);
        }
    };
    for t in thresholds(&versions5) {
        push(Exec {
            flavor: ExecFlavor::MySql,
            six_digit_versions: false,
            server_version: t,
        });
        push(Exec {
            flavor: ExecFlavor::TiDb,
            six_digit_versions: false,
            server_version: t,
        });
    }
    if !versions6.is_empty() {
        for t in thresholds(&all) {
            push(Exec {
                flavor: ExecFlavor::MySql,
                six_digit_versions: true,
                server_version: t,
            });
        }
    }
    for t in thresholds(&all) {
        push(Exec {
            flavor: ExecFlavor::MariaDb,
            six_digit_versions: true,
            server_version: t,
        });
    }
    out
}
