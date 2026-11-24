//! Mutation test over every plan parser: each fixture under
//! `tests/golden/plans/<engine>/` is truncated, spliced, garbled and given
//! absurd numbers, then fed to the auto-detecting import path and to every
//! format parser directly. A parser may reject a document; it may not panic
//! and it may not take seconds.
//!
//! The run is deterministic: the mutations of a fixture come from a
//! xorshift generator seeded by the fixture's name and the iteration number,
//! so a failure names everything needed to replay it
//! (`replay_one_mutation` below).

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use sqlly_intel::plans::dialects::{
    extract_sqlserver_message_stats, parse_clickhouse_explain_json,
    parse_clickhouse_explain_pipeline, parse_clickhouse_explain_text, parse_cockroach_explain_text,
    parse_duckdb_explain_json, parse_duckdb_explain_text, parse_mysql_explain_analyze_tree,
    parse_mysql_explain_json, parse_mysql_explain_json_v2, parse_mysql_explain_table,
    parse_oracle_plan_lines, parse_oracle_xplan_plans, parse_pg_explain_json_multi,
    parse_pg_explain_text, parse_pg_explain_xml, parse_pg_explain_yaml, parse_spark_explain_text,
    parse_sqlite_query_plan_any, parse_sqlite_query_plan_text, parse_sqlserver_showplan_text,
    parse_trino_explain_text,
};
use sqlly_intel::plans::{
    PlanDialect, PlanEngine, clean_plan_text, decode_plan_bytes, detect_plan_format,
    import_plan_document, import_plan_document_as, parse_plan_document, parse_showplan_xml_multi,
    sniff_plan_document,
};

/// Mutated documents per fixture in the always-on test (about 240 fixtures,
/// so a little over three thousand documents in total).
const ITERATIONS: u64 = 13;
/// Mutated documents per fixture in the `#[ignore]`d long run.
const LONG_ITERATIONS: u64 = 350;
/// No single parser call may take longer than this, even in a debug build
/// on a loaded machine.
const TIME_LIMIT: Duration = Duration::from_secs(2);
const BASE_SEED: u64 = 0x005E_ED0F_9E37_79B9;

/// xorshift64*: small, fast, and the same everywhere.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// A number in `0..bound` (0 when `bound` is 0).
    fn below(&mut self, bound: usize) -> usize {
        if bound == 0 {
            0
        } else {
            (self.next() % bound as u64) as usize
        }
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items.get(self.below(items.len())).copied().unwrap_or("")
    }
}

/// FNV-1a, so a fixture's seed does not depend on the order of the files.
fn name_hash(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn seed_of(fixture: &str, iteration: u64) -> u64 {
    BASE_SEED ^ name_hash(fixture) ^ iteration.wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

/// The nearest char boundary at or below `at`.
fn boundary(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

/// A random span of `text` on char boundaries, at most `longest` bytes.
fn span(text: &str, rng: &mut Rng, longest: usize) -> (usize, usize) {
    let start = boundary(text, rng.below(text.len() + 1));
    let length = rng.below(longest.min(text.len() - start) + 1);
    (start, boundary(text, start + length).max(start))
}

const PUNCTUATION: [&str; 24] = [
    "|", "+", "-", "(", ")", "[", "]", "{", "}", "<", ">", "\"", "'", "&", ";", ":", ",", ".", "=",
    "\n", "\t", "\0", "\u{e9}", "\u{2192}",
];
const NUMBERS: [&str; 8] = [
    "99999999999999999999",
    "1e999",
    "-",
    "-1",
    "0",
    "NaN",
    "18446744073709551616",
    "4294967296",
];

/// Apply one random mutation and return its name.
fn mutate(text: &mut String, rng: &mut Rng) -> &'static str {
    match rng.below(8) {
        0 => {
            let cut = boundary(text, rng.below(text.len() + 1));
            text.truncate(cut);
            "truncate"
        }
        1 => {
            let (start, end) = span(text, rng, 400);
            text.replace_range(start..end, "");
            "delete span"
        }
        2 => {
            let (start, end) = span(text, rng, 400);
            let copy = text.get(start..end).unwrap_or_default().to_string();
            // Now and then many copies, to grow a structure.
            let times = if rng.below(6) == 0 { 40 } else { 1 };
            text.insert_str(end, &copy.repeat(times));
            "duplicate span"
        }
        3 => {
            let mut lines: Vec<&str> = text.split('\n').collect();
            if lines.len() > 1 {
                let a = rng.below(lines.len());
                let b = rng.below(lines.len());
                lines.swap(a, b);
            }
            *text = lines.join("\n");
            "swap lines"
        }
        4 | 5 => {
            // Several characters at once: one alone rarely lands on
            // structure.
            for _ in 0..=rng.below(4) {
                let start = boundary(text, rng.below(text.len() + 1));
                let end = text
                    .get(start..)
                    .and_then(|rest| rest.chars().next())
                    .map_or(start, |c| start + c.len_utf8());
                text.replace_range(start..end, rng.pick(&PUNCTUATION));
            }
            "replace characters"
        }
        6 => {
            let from = boundary(text, rng.below(text.len() + 1));
            let digit = text
                .get(from..)
                .and_then(|rest| rest.find(|c: char| c.is_ascii_digit()))
                .map(|offset| from + offset);
            if let Some(start) = digit {
                let length = text
                    .get(start..)
                    .and_then(|rest| rest.find(|c: char| !c.is_ascii_digit()))
                    .unwrap_or(text.len() - start);
                text.replace_range(start..start + length, rng.pick(&NUMBERS));
            }
            "replace number"
        }
        _ => {
            // Shift a line's indentation: structure in most text plans.
            let start = boundary(text, rng.below(text.len() + 1));
            let line_start = text
                .get(..start)
                .and_then(|head| head.rfind('\n'))
                .map_or(0, |at| at + 1);
            if rng.below(2) == 0 {
                text.insert_str(line_start, &" ".repeat(1 + rng.below(12)));
            } else {
                let indent = text
                    .get(line_start..)
                    .map_or(0, |line| line.len() - line.trim_start_matches(' ').len());
                text.replace_range(line_start..line_start + rng.below(indent + 1), "");
            }
            "shift indentation"
        }
    }
}

/// The document for `(fixture, iteration)`, and what was done to it.
/// Iteration 0 is the fixture unchanged.
fn mutated(fixture: &str, original: &str, iteration: u64) -> (String, String) {
    let mut text = original.to_string();
    if iteration == 0 {
        return (text, "unchanged".to_string());
    }
    let mut rng = Rng::new(seed_of(fixture, iteration));
    let mut applied = Vec::new();
    for _ in 0..=rng.below(3) {
        applied.push(mutate(&mut text, &mut rng));
    }
    (text, applied.join(", "))
}

type Target = (&'static str, fn(&str));

const ENGINES: [PlanEngine; 11] = [
    PlanEngine::SqlServer,
    PlanEngine::Postgres,
    PlanEngine::MySql,
    PlanEngine::MariaDb,
    PlanEngine::Sqlite,
    PlanEngine::DuckDb,
    PlanEngine::ClickHouse,
    PlanEngine::Oracle,
    PlanEngine::CockroachDb,
    PlanEngine::Trino,
    PlanEngine::Spark,
];

const DIALECTS: [PlanDialect; 8] = [
    PlanDialect::SqlServer,
    PlanDialect::Postgres,
    PlanDialect::MySql,
    PlanDialect::MariaDb,
    PlanDialect::Sqlite,
    PlanDialect::DuckDb,
    PlanDialect::ClickHouse,
    PlanDialect::Oracle,
];

/// Every way into the parsers. The import paths get the raw text; the format
/// parsers get it cleaned, as the import path hands it to them.
const TARGETS: [Target; 27] = [
    ("import_plan_document", |text| {
        let _ = import_plan_document(text);
    }),
    ("detect_plan_format", |text| {
        let _ = detect_plan_format(&clean_plan_text(text));
    }),
    ("sniff_plan_document", |text| {
        let _ = sniff_plan_document(&clean_plan_text(text));
    }),
    ("decode_plan_bytes", |text| {
        let _ = decode_plan_bytes(text.as_bytes());
    }),
    ("extract_sqlserver_message_stats", |text| {
        let _ = extract_sqlserver_message_stats(text);
    }),
    ("parse_showplan_xml_multi", |text| {
        let _ = parse_showplan_xml_multi(&clean_plan_text(text));
    }),
    ("parse_sqlserver_showplan_text", |text| {
        let _ = parse_sqlserver_showplan_text(&clean_plan_text(text));
    }),
    ("parse_pg_explain_text", |text| {
        let _ = parse_pg_explain_text(&clean_plan_text(text));
    }),
    ("parse_pg_explain_json_multi", |text| {
        let _ = parse_pg_explain_json_multi(&clean_plan_text(text));
    }),
    ("parse_pg_explain_yaml", |text| {
        let _ = parse_pg_explain_yaml(&clean_plan_text(text));
    }),
    ("parse_pg_explain_xml", |text| {
        let _ = parse_pg_explain_xml(&clean_plan_text(text));
    }),
    ("parse_mysql_explain_table", |text| {
        let _ = parse_mysql_explain_table(&clean_plan_text(text));
    }),
    ("parse_mysql_explain_analyze_tree", |text| {
        let _ = parse_mysql_explain_analyze_tree(&clean_plan_text(text));
    }),
    ("parse_mysql_explain_json", |text| {
        let _ = parse_mysql_explain_json(&clean_plan_text(text));
    }),
    ("parse_mysql_explain_json_v2", |text| {
        let _ = parse_mysql_explain_json_v2(&clean_plan_text(text));
    }),
    ("parse_oracle_xplan_plans", |text| {
        let _ = parse_oracle_xplan_plans(&clean_plan_text(text));
    }),
    ("parse_oracle_plan_lines", |text| {
        let _ = parse_oracle_plan_lines(&clean_plan_text(text));
    }),
    ("parse_sqlite_query_plan_any", |text| {
        let _ = parse_sqlite_query_plan_any(&clean_plan_text(text));
    }),
    ("parse_sqlite_query_plan_text", |text| {
        let _ = parse_sqlite_query_plan_text(&clean_plan_text(text));
    }),
    ("parse_duckdb_explain_text", |text| {
        let _ = parse_duckdb_explain_text(&clean_plan_text(text));
    }),
    ("parse_duckdb_explain_json", |text| {
        let _ = parse_duckdb_explain_json(&clean_plan_text(text));
    }),
    ("parse_clickhouse_explain_text", |text| {
        let _ = parse_clickhouse_explain_text(&clean_plan_text(text));
    }),
    ("parse_clickhouse_explain_pipeline", |text| {
        let _ = parse_clickhouse_explain_pipeline(&clean_plan_text(text));
    }),
    ("parse_clickhouse_explain_json", |text| {
        let _ = parse_clickhouse_explain_json(&clean_plan_text(text));
    }),
    ("parse_cockroach_explain_text", |text| {
        let _ = parse_cockroach_explain_text(&clean_plan_text(text));
    }),
    ("parse_trino_explain_text", |text| {
        let _ = parse_trino_explain_text(&clean_plan_text(text));
    }),
    ("parse_spark_explain_text", |text| {
        let _ = parse_spark_explain_text(&clean_plan_text(text));
    }),
];

/// Every fixture as `(engine/file, decoded text)`, sorted.
fn fixtures() -> Vec<(String, String)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/golden/plans");
    let mut out = Vec::new();
    let mut folders: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("fixture folder")
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.is_dir())
        .collect();
    folders.sort();
    for folder in folders {
        let engine = folder
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&folder)
            .expect("engine folder")
            .filter_map(|entry| entry.ok().map(|entry| entry.path()))
            .filter(|path| {
                path.is_file()
                    && path.extension().is_none_or(|ext| ext != "golden")
                    && !path
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with('.'))
            })
            .collect();
        files.sort();
        for file in files {
            let name = file
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let text = decode_plan_bytes(&std::fs::read(&file).expect("fixture"));
            out.push((format!("{engine}/{name}"), text));
        }
    }
    out
}

fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| payload.downcast_ref::<&str>().map(|text| text.to_string()))
        .unwrap_or_else(|| "panic without a message".to_string())
}

/// Run `call` and describe what went wrong, if anything did.
fn check(name: &str, call: impl FnOnce()) -> Option<String> {
    let started = Instant::now();
    let outcome = catch_unwind(AssertUnwindSafe(call));
    let took = started.elapsed();
    match outcome {
        Err(payload) => Some(format!("{name} panicked: {}", panic_text(payload.as_ref()))),
        Ok(()) if took > TIME_LIMIT => Some(format!("{name} took {took:?}")),
        Ok(()) => None,
    }
}

/// Feed one document to every parser. Returns one line per failure.
fn run_document(fixture: &str, iteration: u64, text: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for (name, target) in TARGETS {
        problems.extend(check(name, || target(text)));
    }
    // The paths that take an engine: the fixture's pick changes per
    // iteration so every engine sees every kind of document.
    let pick = seed_of(fixture, iteration) as usize;
    if let Some(engine) = ENGINES.get(pick % ENGINES.len()).copied() {
        problems.extend(check(
            &format!("import_plan_document_as({engine:?})"),
            || {
                let _ = import_plan_document_as(engine, text);
            },
        ));
    }
    if let Some(dialect) = DIALECTS.get((pick / 16) % DIALECTS.len()).copied() {
        problems.extend(check(&format!("parse_plan_document({dialect:?})"), || {
            let _ = parse_plan_document(dialect, &clean_plan_text(text));
        }));
    }
    problems
}

/// Run `iterations` mutated documents per fixture over every parser, on all
/// cores. Returns the failures.
fn fuzz(iterations: u64) -> Vec<String> {
    let fixtures = fixtures();
    assert!(
        fixtures.len() > 100,
        "expected the plan fixtures, found {}",
        fixtures.len()
    );
    let failures = Mutex::new(Vec::new());
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    // The default hook prints every caught panic; the report below says
    // more.
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some((fixture, original)) = fixtures.get(index) else {
                        break;
                    };
                    for iteration in 0..=iterations {
                        let (text, mutation) = mutated(fixture, original, iteration);
                        for problem in run_document(fixture, iteration, &text) {
                            let line =
                                format!("{fixture} iteration {iteration} ({mutation}): {problem}");
                            if let Ok(mut failures) = failures.lock() {
                                failures.push(line);
                            }
                        }
                    }
                }
            });
        }
    });
    std::panic::set_hook(hook);
    let mut failures = failures.into_inner().unwrap_or_default();
    failures.sort();
    failures
}

fn report(failures: &[String]) {
    assert!(
        failures.is_empty(),
        "{} parser failures. Replay one with PLAN_FUZZ_FIXTURE=<engine/file> PLAN_FUZZ_ITERATION=<n> and the replay_one_mutation test:\n{}",
        failures.len(),
        failures
            .iter()
            .take(60)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}

#[test]
fn mutated_fixtures_never_panic_or_stall_a_parser() {
    report(&fuzz(ITERATIONS));
}

/// The same with many more documents per fixture. Run it after changing a
/// parser: `cargo test -p sqlly-intel --test plan_parser_fuzz -- --ignored`.
#[test]
#[ignore = "long run; the always-on test covers the same paths with fewer documents"]
fn mutated_fixtures_never_panic_or_stall_a_parser_long_run() {
    report(&fuzz(LONG_ITERATIONS));
}

/// Replay one document of the run without catching the panic, so the
/// backtrace points at the parser:
/// `PLAN_FUZZ_FIXTURE=duckdb/duckdb142-join-analyze.txt PLAN_FUZZ_ITERATION=7
/// cargo test -p sqlly-intel --test plan_parser_fuzz replay -- --ignored --nocapture`
#[test]
#[ignore = "debugging aid; needs PLAN_FUZZ_FIXTURE and PLAN_FUZZ_ITERATION"]
fn replay_one_mutation() {
    let fixture = std::env::var("PLAN_FUZZ_FIXTURE").expect("PLAN_FUZZ_FIXTURE");
    let iteration: u64 = std::env::var("PLAN_FUZZ_ITERATION")
        .expect("PLAN_FUZZ_ITERATION")
        .parse()
        .expect("iteration number");
    let fixtures = fixtures();
    let (_, original) = fixtures
        .iter()
        .find(|(name, _)| *name == fixture)
        .expect("fixture exists");
    let (text, mutation) = mutated(&fixture, original, iteration);
    eprintln!("mutation: {mutation}\n---\n{text}\n---");
    for (name, target) in TARGETS {
        eprintln!("running {name}");
        target(&text);
    }
    for engine in ENGINES {
        let _ = import_plan_document_as(engine, &text);
    }
    for dialect in DIALECTS {
        let _ = parse_plan_document(dialect, &clean_plan_text(&text));
    }
}

/// Nesting deeper than any real plan must not overflow the stack: every
/// parser walks its tree with a bounded depth or without recursion.
#[test]
fn absurdly_deep_plans_do_not_overflow_the_stack() {
    // XML and JSON nest without indentation, so they can go very deep. The
    // text formats spend one column per level, so a document's size grows
    // with the square of its depth; 600 levels is 180 KB.
    let depth = 20_000;
    let text_depth = 600;
    let mut documents: Vec<(&str, String)> = Vec::new();

    let mut xml = String::from("<ShowPlanXML><StmtSimple StatementText=\"SELECT 1\">");
    for id in 0..depth {
        xml.push_str(&format!(
            "<RelOp NodeId=\"{id}\" PhysicalOp=\"Filter\" LogicalOp=\"Filter\"><Filter>"
        ));
    }
    for _ in 0..depth {
        xml.push_str("</Filter></RelOp>");
    }
    xml.push_str("</StmtSimple></ShowPlanXML>");
    documents.push(("showplan xml", xml));

    let mut showplan = String::from("SELECT 1\n");
    let mut pg = String::new();
    let mut mysql = String::new();
    let mut sqlite = String::from("QUERY PLAN\n");
    let mut clickhouse = String::new();
    let mut trino = String::new();
    let mut spark = String::from("== Physical Plan ==\n");
    let mut cockroach = String::new();
    for level in 0..text_depth {
        let pad = " ".repeat(level);
        showplan.push_str(&format!("{pad}|--Filter(WHERE:([a]=(1)))\n"));
        pg.push_str(&format!(
            "{pad}->  Seq Scan on t{level}  (cost=0.00..1.00 rows=1 width=4)\n"
        ));
        mysql.push_str(&format!("{pad}-> Filter: (t.a = 1)  (cost=1.00 rows=1)\n"));
        sqlite.push_str(&format!("{pad}`--SCAN t{level}\n"));
        clickhouse.push_str(&format!("{pad}Expression (Projection)\n"));
        trino.push_str(&format!("{pad}- Filter[filterPredicate = (a = 1)]\n"));
        spark.push_str(&format!("{pad}+- Filter (a#1 = 1)\n"));
        cockroach.push_str(&format!("{pad}\u{2514}\u{2500}\u{2500} \u{2022} filter\n"));
    }
    documents.push(("showplan text", showplan));
    documents.push(("pg text", pg));
    documents.push(("mysql tree", mysql));
    documents.push(("sqlite", sqlite));
    documents.push(("clickhouse text", clickhouse));
    documents.push(("trino", trino));
    documents.push(("spark", spark));
    documents.push(("cockroach", cockroach));

    // Oracle marks depth with one space per level in the Operation column.
    let mut oracle = String::from(
        "Plan hash value: 1\n\n--------------------------------------------\n| Id  | Operation | Name | Rows |\n--------------------------------------------\n",
    );
    for level in 0..text_depth {
        oracle.push_str(&format!(
            "| {level:>3} | {}FILTER | | 1 |\n",
            " ".repeat(level)
        ));
    }
    oracle.push_str("--------------------------------------------\n");
    documents.push(("oracle xplan", oracle));

    // JSON nested past serde_json's own limit is rejected, not walked.
    for (name, open, close) in [
        (
            "pg json",
            "[{\"Plan\": {\"Node Type\": \"Result\", \"Plans\": [",
            "]}}]",
        ),
        (
            "duckdb json",
            "[{\"name\": \"FILTER\", \"children\": [",
            "]}]",
        ),
        ("mysql json", "{\"query_block\": {\"nested_loop\": [", "]}}"),
    ] {
        let text = format!("{}{}", open.repeat(5_000), close.repeat(5_000));
        documents.push((name, text));
    }

    // A small stack makes an unbounded recursion fail here instead of only
    // on some platform with a smaller default.
    let worker = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            for (name, text) in &documents {
                let started = Instant::now();
                for (target_name, target) in TARGETS {
                    let one = Instant::now();
                    target(text);
                    if std::env::var_os("PLAN_FUZZ_TIMES").is_some() {
                        eprintln!("{name} / {target_name}: {:?}", one.elapsed());
                    }
                }
                let took = started.elapsed();
                assert!(
                    took < Duration::from_secs(20),
                    "{name}: deep document took {took:?}"
                );
            }
        })
        .expect("thread starts");
    worker.join().expect("no parser panics on a deep document");
}
