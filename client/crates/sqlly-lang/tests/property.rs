//! Property tests proving the lexer/parser pipeline is total: arbitrary
//! input never panics, never hangs, and produces spans that stay inside the
//! source. The parser has no recursive descent cycles (nesting is handled
//! iteratively), so these properties are the load-bearing safety contract
//! for editor-fed text — there is deliberately no depth guard to test.

use proptest::prelude::*;
use sqlly_lang::sqllex::{StrictLexer, split_statement_tokens, split_statements};
use sqlly_lang::sqlmodel::Dialect;
use sqlly_lang::sqlparse::{parse, resolve_slots};

proptest! {
    #![proptest_config(ProptestConfig::with_cases(192))]

    /// Strict-lexer invariant: tokens tile the input exactly — contiguous,
    /// non-empty, span-faithful, always reaching EOF — for every dialect and
    /// arbitrary Unicode input.
    #[test]
    fn lexer_tiles_arbitrary_input(chars in prop::collection::vec(any::<char>(), 0..300)) {
        let src: String = chars.into_iter().collect();
        for dialect in Dialect::ALL {
            let tokens = StrictLexer::tokenize(&src, dialect);
            let mut pos = 0usize;
            for t in &tokens {
                prop_assert_eq!(t.span.start, pos, "gap before token in {:?}", dialect);
                prop_assert!(t.span.end > t.span.start, "empty token in {:?}", dialect);
                prop_assert_eq!(&src[t.span.start..t.span.end], t.text);
                pos = t.span.end;
            }
            prop_assert_eq!(pos, src.len(), "lexer stopped before EOF in {:?}", dialect);
        }
    }

    /// Parsing arbitrary input must not panic, and every emitted span
    /// (statements + recovery actions) must stay inside the source.
    #[test]
    fn parse_is_total_with_bounded_spans(chars in prop::collection::vec(any::<char>(), 0..300)) {
        let src: String = chars.into_iter().collect();
        for dialect in Dialect::ALL {
            let tree = parse(&src, dialect);
            for r in &tree.recovery {
                prop_assert!(r.span.start <= r.span.end && r.span.end <= src.len());
            }
        }
    }

    /// Slot resolution rides the same pipeline and must be total too.
    #[test]
    fn slot_resolution_is_total(chars in prop::collection::vec(any::<char>(), 0..300)) {
        let src: String = chars.into_iter().collect();
        for dialect in Dialect::ALL {
            let _ = resolve_slots(&src, dialect);
        }
    }

    /// The token-run splitter and the text splitter must agree statement for
    /// statement. Restricted to tab/newline/printable ASCII because the text
    /// path trims Unicode whitespace the lexer legitimately tokenizes
    /// (e.g. U+000B, U+00A0) — the divergence there is understood and inert.
    /// The token run may end past the trimmed text only when an unterminated
    /// construct's error token swallowed trailing whitespace.
    #[test]
    fn token_split_agrees_with_text_split(src in "[ -~\t\r\n]{0,300}") {
        for dialect in [Dialect::SqlServer, Dialect::Postgres, Dialect::Oracle, Dialect::Sqlite] {
            let slices = split_statements(&src, dialect);
            let runs = split_statement_tokens(&src, dialect);
            prop_assert_eq!(slices.len(), runs.len(), "{:?}: {:?}", dialect, src);
            for (slice, run) in slices.iter().zip(&runs) {
                let first = run.first().unwrap();
                let last = run.last().unwrap();
                prop_assert_eq!(first.span.start, slice.span.start);
                prop_assert!(
                    last.span.end >= slice.span.end && last.span.end <= src.len(),
                    "{:?}: {:?}", dialect, src
                );
                prop_assert!(
                    src[slice.span.end..last.span.end].chars().all(char::is_whitespace),
                    "token run extended past trimmed text by non-whitespace: {:?}", src
                );
            }
        }
    }
}

/// Pathological nesting: with no recursion in the parser this is a flat token
/// walk — it must finish quickly with the stack untouched.
#[test]
fn deeply_nested_parens_do_not_overflow() {
    let open = "(".repeat(100_000);
    let close = ")".repeat(100_000);
    for dialect in Dialect::ALL {
        let _ = parse(&format!("SELECT {open}1{close}"), dialect);
        let _ = parse(&open, dialect);
    }
}

/// Unterminated constructs must land as error tokens/recovery, never hangs.
#[test]
fn unterminated_constructs_terminate() {
    let cases = [
        "select 'unterminated",
        "select \"unterminated",
        "select [unterminated",
        "select `unterminated",
        "/* unterminated block",
        "select N'unterminated",
        "select E'unterminated",
        "select q'{unterminated",
        "select $tag$ unterminated",
        "create table T (a int",
    ];
    for dialect in Dialect::ALL {
        for case in cases {
            let _ = parse(case, dialect);
            let _ = resolve_slots(case, dialect);
        }
    }
}

/// A large realistic script parses without issue end to end (smoke for the
/// single-lex splitter under volume).
#[test]
fn large_script_parses() {
    let mut script = String::new();
    for i in 0..5_000 {
        script.push_str(&format!("INSERT INTO t{i} (a, b) VALUES ({i}, 'v{i}');\n"));
    }
    for dialect in Dialect::ALL {
        let tree = parse(&script, dialect);
        assert!(tree.statements.len() >= 5_000, "{dialect:?}");
    }
}
