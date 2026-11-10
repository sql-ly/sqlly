//! Coarse latency guard for the parse + slot-resolution path, in the style of
//! the tokenizer's `tokenizer_stays_within_latency_budget`: a generous
//! order-of-magnitude budget that catches an accidental quadratic blowup or a
//! reintroduced per-statement re-lex under plain `cargo test`, deliberately
//! NOT a criterion gate (exact numbers live in `cargo bench`).

use sqlly_lang::sqlmodel::Dialect;
use std::time::Instant;

fn generated_script(statement_count: usize) -> String {
    let mut sql = String::with_capacity(statement_count * 220);
    for i in 0..statement_count {
        sql.push_str(&format!(
            "CREATE TABLE dbo.BudgetTable{i:04} (Id INT NOT NULL PRIMARY KEY, ParentId INT, Name NVARCHAR(100));\n"
        ));
        sql.push_str(&format!(
            "UPDATE dbo.BudgetTable{i:04} SET Name = 'v' WHERE Id = {i} AND ParentId = {i};\n"
        ));
    }
    sql
}

#[test]
fn parse_and_slot_resolution_stay_within_latency_budget() {
    let sql = generated_script(1_000);
    let bytes = sql.len();

    let start = Instant::now();
    let tree = sqlly_lang::sqlparse::parse(&sql, Dialect::SqlServer);
    let parse_elapsed = start.elapsed();
    assert_eq!(tree.statements.len(), 2_000, "all statements parsed");

    let start = Instant::now();
    let slots = sqlly_lang::sqlparse::resolve_slots(&sql, Dialect::SqlServer);
    let slots_elapsed = start.elapsed();
    assert!(!slots.is_empty(), "the UPDATE predicates yield slots");

    // Release-mode parse of this script is ~2ms; debug is tens of ms. The
    // budget only exists to catch order-of-magnitude regressions.
    assert!(
        parse_elapsed.as_millis() < 2_000,
        "parsing {bytes} bytes took {parse_elapsed:?}, over the 2000ms budget"
    );
    assert!(
        slots_elapsed.as_millis() < 2_000,
        "slot resolution over {bytes} bytes took {slots_elapsed:?}, over the 2000ms budget"
    );
}
