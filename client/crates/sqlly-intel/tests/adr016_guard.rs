//! ADR-016 mechanical tripwire (as amended 2026-09-28): safety decisions read
//! SQL only through the security lexer (`sqlly_lang::seclex`), never through
//! the editor's dialect lexer (`sqllex`, one reading per *parser family*) and
//! never by tokenizing raw SQL directly (the dialect-free tokenizer has no
//! quoting rules for backticks, brackets-vs-arrays, `$$`, `#` comments, … so
//! it cannot bound a literal the way any real engine does). The behavioral
//! tripwires live in `query::safety::bypass_tests` and `seclex::tests`; this
//! test pins how the code is allowed to be built.
//!
//! The invariant surfaces used to live in crates of their own, so the check
//! could read a Cargo.toml and be done. They are modules now, sharing a crate
//! with the dialect-aware lexer they must not use, so the check reads the
//! module sources instead: a `sqllex` path anywhere inside them is the merge
//! ADR-016 rejects.

use std::path::{Path, PathBuf};

/// The `client/` workspace root, found by walking up from this crate.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    while !dir.join("Cargo.toml").is_file()
        || !std::fs::read_to_string(dir.join("Cargo.toml"))
            .unwrap_or_default()
            .contains("[workspace]")
    {
        dir = dir
            .parent()
            .expect("crate lives under the client workspace")
            .to_path_buf();
    }
    dir
}

fn rust_sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("read {dir:?}: {e}"));
    for entry in entries {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            found.extend(rust_sources(&path));
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
    found
}

/// Neither the query crate's dialect-invariant surfaces nor the filesystem
/// tokenizer may reach the dialect-aware lexer. Routing safety/parameter/
/// diagnostic lexing through it would make safety verdicts vary with the
/// connected database.
#[test]
fn invariant_sources_do_not_reach_the_dialect_lexer() {
    let root = workspace_root();
    let guarded = [
        root.join("crates/sqlly-intel/src/query"),
        root.join("crates/sqlly-lang/src/tokenizer"),
    ];
    for dir in guarded {
        let sources = rust_sources(&dir);
        assert!(!sources.is_empty(), "no .rs sources found under {dir:?}");
        for path in sources {
            let source = std::fs::read_to_string(&path).expect("read source");
            // Comments are stripped: these modules discuss the dialect-aware
            // lexer in prose precisely because they must not use it.
            let code: String = source
                .lines()
                .map(|line| line.split("//").next().unwrap_or(""))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                !code.contains("sqllex"),
                "{path:?} reaches sqllex — revisit ADR-016 before doing this"
            );
        }
    }
}

/// Safety is keyed by *engine* (`DatabaseType` → `seclex::LexEngine`), never
/// by the editor's parser `Dialect`: families are borrowed (ClickHouse parses
/// with the MySQL grammar but quotes differently), so a Dialect-keyed verdict
/// would judge text under the wrong engine's rules. The parameter scanner
/// stays dialect-free.
#[test]
fn safety_and_parameter_sources_stay_dialect_free() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let safety_dir = crate_root.join("src/query/safety");
    let mut sources = rust_sources(&safety_dir);
    assert!(
        !sources.is_empty(),
        "no .rs sources found under {safety_dir:?}"
    );
    sources.push(crate_root.join("src/query/parameters.rs"));
    for path in sources {
        let source = std::fs::read_to_string(&path).expect("read source");
        assert!(
            !source.contains("Dialect"),
            "{path:?} references Dialect — safety is keyed by engine (DatabaseType), see ADR-016"
        );
    }
}

/// Safety sources never tokenize raw SQL themselves: every token they judge
/// comes from a `seclex::View` (`view.tokens()`), which is the text as some
/// real engine reading bounds it.
#[test]
fn safety_sources_read_sql_only_through_the_security_lexer() {
    let crate_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let safety_dir = crate_root.join("src/query/safety");
    for path in rust_sources(&safety_dir) {
        if path.file_name().is_some_and(|n| n == "redis.rs") {
            // Redis is not SQL: its own command-line parser is the model.
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read source");
        let code: String = source
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !code.contains("tokenize("),
            "{path:?} tokenizes SQL directly — read it through seclex views instead (ADR-016)"
        );
    }
}
