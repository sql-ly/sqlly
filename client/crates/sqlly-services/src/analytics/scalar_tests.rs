use sqlly_core::{CellValue, ResultColumn};

use super::*;
use crate::analytics::test_support::*;
use crate::analytics::{Completeness, OutputView};

struct Fixture {
    orders_columns: Vec<ResultColumn>,
    orders_rows: Vec<Vec<CellValue>>,
    total_columns: Vec<ResultColumn>,
    total_rows: Vec<Vec<CellValue>>,
}

fn fixture() -> Fixture {
    Fixture {
        orders_columns: vec![
            col("id", "int"),
            col("amount", "decimal"),
            col("Order Date", "date"),
            col("region", "text"),
        ],
        orders_rows: vec![
            vec![i(1), s("10.50"), s("2024-01-01"), s("east")],
            vec![i(2), f(20.0), s("2024-01-15"), s("west")],
            vec![i(3), CellValue::Null, s("2024-02-01"), s("east")],
            vec![i(4), i(30), s("2024-03-01"), CellValue::Null],
        ],
        total_columns: vec![col("total", "int")],
        total_rows: vec![vec![i(1_000)]],
    }
}

fn env(fixture: &Fixture) -> MapEnv<'_> {
    let mut env = MapEnv::default();
    env.outputs.insert(
        "orders".into(),
        OutputView::complete(&fixture.orders_columns, &fixture.orders_rows),
    );
    env.outputs.insert(
        "target".into(),
        OutputView::complete(&fixture.total_columns, &fixture.total_rows),
    );
    env.values
        .insert("param.region".into(), ScalarValue::Text("east".into()));
    env.values.insert("growth".into(), ScalarValue::Number(0.1));
    env
}

fn num(n: f64) -> ScalarValue {
    ScalarValue::Number(n)
}

/// Any numeric value as a float, for the table's tolerance check.
fn number_of(value: &ScalarValue) -> Option<f64> {
    match value {
        ScalarValue::Number(n) => Some(*n),
        ScalarValue::Integer(n) => Some(*n as f64),
        ScalarValue::Decimal(d) => d.parse().ok(),
        _ => None,
    }
}

fn text(t: &str) -> ScalarValue {
    ScalarValue::Text(t.to_string())
}

#[test]
fn expression_table() {
    let fixture = fixture();
    let env = env(&fixture);
    let date = |t: &str| ScalarValue::Date(parse_timestamp(t).unwrap());
    let cases: Vec<(&str, ScalarValue)> = vec![
        ("1 + 2 * 3", num(7.0)),
        ("(1 + 2) * 3", num(9.0)),
        ("-2 ^ 2", num(-4.0)),
        ("2 ^ 3 ^ 2", num(512.0)),
        ("7 % 3", num(1.0)),
        ("10 / 4", num(2.5)),
        ("'a' || 'b' || 1", text("ab1")),
        ("'it''s'", text("it's")),
        ("1 = 1.0", ScalarValue::Bool(true)),
        ("'10' > 9", ScalarValue::Bool(true)),
        ("'b' > 'a' and not false", ScalarValue::Bool(true)),
        ("null + 1", ScalarValue::Null),
        ("null = null", ScalarValue::Null),
        ("false and null", ScalarValue::Bool(false)),
        ("true or null", ScalarValue::Bool(true)),
        ("true and null", ScalarValue::Null),
        ("if(1 > 2, 'yes', 'no')", text("no")),
        ("if(null, 1, 2)", num(2.0)),
        ("if(false, 1)", ScalarValue::Null),
        ("coalesce(null, null, 3)", num(3.0)),
        ("nullif(5, 5)", ScalarValue::Null),
        ("round(2.675, 2)", num(2.68)),
        ("round(-2.5)", num(-3.0)),
        ("round(1234, -2)", num(1200.0)),
        ("floor(-1.5) + ceil(1.2) + abs(-3)", num(3.0)),
        ("sign(-4)", num(-1.0)),
        ("sqrt(16) + pow(2, 10)", num(1028.0)),
        ("min(3, 1, 2)", num(1.0)),
        ("max(3, null, 2)", num(3.0)),
        ("sum(orders.amount)", num(60.5)),
        ("avg(orders.amount)", num(60.5 / 3.0)),
        ("count(orders.amount)", num(3.0)),
        ("count(orders)", num(4.0)),
        ("count_distinct(orders.region)", num(2.0)),
        ("max(orders.id) - min(orders.id)", num(3.0)),
        ("median(orders.amount)", num(20.0)),
        ("first(orders.amount) + last(orders.amount)", num(40.5)),
        ("orders.amount[1]", num(20.0)),
        ("orders.id[-1]", num(4.0)),
        ("orders.\"Order Date\"[0]", text("2024-01-01")),
        ("target", num(1_000.0)),
        ("target.total * (1 + growth)", num(1_100.0)),
        ("param.region", text("east")),
        ("param.region = orders.region[0]", ScalarValue::Bool(true)),
        ("safe_div(1, 0)", ScalarValue::Null),
        ("pct_change(110, 100)", num(10.0)),
        ("date_diff('day', '2024-01-01', '2024-03-01')", num(60.0)),
        (
            "date_diff('months', orders.\"Order Date\"[0], orders.\"Order Date\"[-1])",
            num(2.0),
        ),
        ("date_add('month', 1, '2024-01-31')", date("2024-02-29")),
        ("date('2024-05-01') > '2024-04-30'", ScalarValue::Bool(true)),
        (
            "year('2024-05-01') * 100 + month('2024-05-01')",
            num(202_405.0),
        ),
        ("len('日本語') + len(upper('ß'))", num(5.0)),
        ("lower('ÀB') || trim('  x ')", text("àbx")),
        ("concat('a', null, 1, true)", text("a1true")),
        ("sum(1, '2.5', null)", num(3.5)),
    ];
    for (source, expected) in cases {
        let value = eval_str(source, &env).unwrap_or_else(|error| panic!("{source}: {error:?}"));
        match (number_of(&value), &expected) {
            (Some(a), ScalarValue::Number(b)) => {
                assert!((a - b).abs() < 1e-9, "{source}: {a} vs {b}")
            }
            _ => assert_eq!(value, expected, "{source}"),
        }
    }
}

#[test]
fn errors_are_typed() {
    let fixture = fixture();
    let env = env(&fixture);
    let cases: Vec<(&str, ScalarError)> = vec![
        ("1 / 0", ScalarError::DivisionByZero),
        ("5 % 0", ScalarError::DivisionByZero),
        (
            "missing",
            ScalarError::UnknownReference {
                name: "missing".into(),
            },
        ),
        (
            "orders.nope",
            ScalarError::UnknownColumn {
                output: "orders".into(),
                column: "nope".into(),
            },
        ),
        (
            "orders.amount",
            ScalarError::NotScalar {
                reference: "orders.amount".into(),
                rows: 4,
            },
        ),
        (
            "orders",
            ScalarError::NotScalar {
                reference: "orders".into(),
                rows: 4,
            },
        ),
        (
            "orders.amount[10]",
            ScalarError::RowOutOfRange {
                reference: "orders.amount".into(),
                row: 10,
            },
        ),
        (
            "frobnicate(1)",
            ScalarError::UnknownFunction {
                name: "frobnicate".into(),
            },
        ),
        (
            "round()",
            ScalarError::WrongArgumentCount {
                function: "round".into(),
                expected: "1-2".into(),
            },
        ),
        (
            "date_diff('fortnight', '2024-01-01', '2024-01-02')",
            ScalarError::InvalidUnit {
                unit: "fortnight".into(),
            },
        ),
        (
            "date('soon')",
            ScalarError::InvalidDate {
                text: "soon".into(),
            },
        ),
        (
            "'abc' + 1",
            ScalarError::TypeMismatch {
                context: "+".into(),
                expected: ValueKind::Number,
                found: ValueKind::Text,
            },
        ),
    ];
    for (source, expected) in cases {
        assert_eq!(eval_str(source, &env), Err(expected), "{source}");
    }
    for source in [
        "1 +", "(1", "'open", "\"open", "1 2", "a.[0]", "a[x]", "#", "and",
    ] {
        assert!(
            matches!(parse(source), Err(ScalarError::Parse { .. })),
            "{source}: {:?}",
            parse(source)
        );
    }
    assert_eq!(
        parse("1 + ?"),
        Err(ScalarError::Parse {
            at: 4,
            error: ParseErrorKind::UnexpectedChar { ch: '?' }
        })
    );
}

#[test]
fn evaluation_is_bounded() {
    let env = MapEnv::default();
    assert_eq!(
        parse(&"1+".repeat(MAX_SOURCE_CHARS)),
        Err(ScalarError::TooLong {
            max_chars: MAX_SOURCE_CHARS
        })
    );
    let deep = format!(
        "{}1{}",
        "(".repeat(MAX_DEPTH + 1),
        ")".repeat(MAX_DEPTH + 1)
    );
    assert_eq!(
        parse(&deep),
        Err(ScalarError::TooDeep {
            max_depth: MAX_DEPTH
        })
    );
    let negations = format!("{}1", "-".repeat(MAX_DEPTH + 1));
    assert_eq!(
        parse(&negations),
        Err(ScalarError::TooDeep {
            max_depth: MAX_DEPTH
        })
    );
    let wide = vec!["1"; MAX_NODES + 1].join("+");
    assert_eq!(
        parse(&wide),
        Err(ScalarError::TooComplex {
            max_nodes: MAX_NODES
        })
    );
    let long_chain = vec!["1"; MAX_TREE_DEPTH + 2].join("+");
    assert_eq!(
        parse(&long_chain),
        Err(ScalarError::TooDeep {
            max_depth: MAX_TREE_DEPTH
        })
    );
    let fine = vec!["1"; 200].join("+");
    assert_eq!(eval_str(&fine, &env), Ok(ScalarValue::Integer(200)));

    // A big output aggregated many times runs out of budget instead of
    // running long.
    let columns = vec![col("v", "int")];
    let rows: Vec<Vec<CellValue>> = (0..1_000_000).map(|n| vec![i(n)]).collect();
    let mut big = MapEnv::default();
    big.outputs
        .insert("big".into(), OutputView::complete(&columns, &rows));
    let source = ["sum(big.v)"; 6].join("+");
    assert_eq!(
        eval_str(&source, &big),
        Err(ScalarError::BudgetExceeded {
            max_steps: MAX_STEPS
        })
    );
    assert_eq!(
        eval_str("sum(big.v)", &big),
        Ok(ScalarValue::Integer(499_999_500_000))
    );
    assert_eq!(
        eval_str(
            &format!(
                "concat({})",
                vec!["'aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa'"; 70].join(",")
            ),
            &env
        )
        .map(|v| v.display().len()),
        Ok(3_500)
    );
}

#[test]
fn references_and_round_trips() {
    let expr =
        parse("sum(orders.amount) / target + param.region + \"My Cell\".x + orders.id[0]").unwrap();
    assert_eq!(
        expr.references(),
        vec!["My Cell", "orders", "param.region", "target"]
    );
    let json = serde_json::to_string(&expr).unwrap();
    assert_eq!(serde_json::from_str::<Expr>(&json).unwrap(), expr);
    let fixture = fixture();
    let env = env(&fixture);
    assert_eq!(
        eval_str("sum(orders.amount)", &env),
        eval_str("sum(orders.amount)", &env)
    );
    let value = ScalarValue::Date(parse_timestamp("2024-01-02").unwrap());
    assert_eq!(value.to_cell(), CellValue::String("2024-01-02".into()));
    assert_eq!(num(3.0).to_cell(), CellValue::I64(3));
    assert_eq!(num(f64::NAN).display(), "NaN");
    assert_eq!(num(0.5).to_cell(), CellValue::F64(0.5));
    let json = serde_json::to_string(&value).unwrap();
    assert_eq!(json, r#"{"type":"date","value":"2024-01-02T00:00:00"}"#);
    assert_eq!(serde_json::from_str::<ScalarValue>(&json).unwrap(), value);
}

#[test]
fn nan_and_infinity_flow_through_arithmetic() {
    let columns = vec![col("x", "float")];
    let rows = vec![vec![f(f64::INFINITY)], vec![f(f64::NAN)], vec![f(2.0)]];
    let mut env = MapEnv::default();
    env.outputs.insert(
        "t".into(),
        OutputView::new(&columns, &rows, Completeness::Complete),
    );
    assert_eq!(
        eval_str("sum(t.x)", &env),
        Ok(num(2.0)),
        "aggregates skip non-finite values"
    );
    let ScalarValue::Number(inf) = eval_str("t.x[0] * 2", &env).unwrap() else {
        panic!()
    };
    assert!(inf.is_infinite());
    let ScalarValue::Number(nan) = eval_str("t.x[1] + 1", &env).unwrap() else {
        panic!()
    };
    assert!(nan.is_nan());
}

#[test]
fn bigints_and_wide_decimals_keep_every_digit() {
    let columns = vec![col("id", "bigint"), col("amount", "decimal(38,2)")];
    let rows = vec![
        vec![i(1234567890123456789), s("12345678901234567.89")],
        vec![i(9007199254740993), s("0.01")],
    ];
    let mut env = MapEnv::default();
    env.outputs
        .insert("o".into(), OutputView::complete(&columns, &rows));
    let exact = |source: &str| eval_str(source, &env).unwrap();
    assert_eq!(exact("o.id[0]"), ScalarValue::Integer(1234567890123456789));
    assert_eq!(
        exact("o.amount[0] + 0"),
        ScalarValue::Decimal("12345678901234567.89".into())
    );
    assert_eq!(
        exact("sum(o.amount)"),
        ScalarValue::Decimal("12345678901234567.90".into())
    );
    assert_eq!(exact("o.id[1] + 1"), ScalarValue::Integer(9007199254740994));
    assert_eq!(
        exact("max(o.id)"),
        ScalarValue::Integer(1234567890123456789)
    );
    assert_eq!(
        exact("first(o.id)"),
        ScalarValue::Integer(1234567890123456789)
    );
    assert_eq!(
        exact("o.id[0] * 10"),
        ScalarValue::Decimal("12345678901234567890".into()),
        "past i64 the product stays exact as a decimal"
    );
    assert_eq!(
        exact("1234567890123456789 - 1"),
        ScalarValue::Integer(1234567890123456788)
    );
    assert_eq!(
        exact("o.id[0] = 1234567890123456788"),
        ScalarValue::Bool(false)
    );
    assert_eq!(
        exact("o.id[0] > 1234567890123456788"),
        ScalarValue::Bool(true)
    );
    assert_eq!(
        exact("round(o.amount[0], 1)"),
        ScalarValue::Decimal("12345678901234567.9".into())
    );
    assert_eq!(exact("0.1 + 0.2 = 0.3"), ScalarValue::Bool(true));

    let precise = evaluate_detailed(&parse("o.id[1] + 1").unwrap(), &env).unwrap();
    assert!(!precise.approximate && !precise.partial);
    let lossy = evaluate_detailed(&parse("o.id[0] / 3").unwrap(), &env).unwrap();
    assert!(matches!(lossy.value, ScalarValue::Number(_)));
    assert!(
        lossy.approximate,
        "float arithmetic on a 19-digit id is flagged"
    );
    let float_ok = evaluate_detailed(&parse("10 / 4").unwrap(), &env).unwrap();
    assert!(!float_ok.approximate);

    let json = serde_json::to_string(&ScalarValue::Integer(1234567890123456789)).unwrap();
    assert_eq!(json, r#"{"type":"integer","value":1234567890123456789}"#);
    assert_eq!(
        serde_json::from_str::<ScalarValue>(r#"{"type":"number","value":2.5}"#).unwrap(),
        num(2.5),
        "values saved before exact numbers still read"
    );
}

#[test]
fn values_read_from_incomplete_outputs_are_partial() {
    let columns = vec![col("amt", "int")];
    let rows = vec![vec![i(10)], vec![i(20)]];
    let mut env = MapEnv::default();
    env.outputs.insert(
        "o".into(),
        OutputView::new(&columns, &rows, Completeness::Truncated { cap: 2 }),
    );
    let run = |source: &str| evaluate_detailed(&parse(source).unwrap(), &env).unwrap();
    let total = run("sum(o.amt)");
    assert_eq!(total.value, ScalarValue::Integer(30));
    assert!(total.partial, "a sum over a truncated output is partial");
    assert!(run("count(o)").partial);
    assert!(
        run("o.amt[-1]").partial,
        "the last kept row is not the last row"
    );
    assert!(
        !run("o.amt[0]").partial,
        "leading rows of a truncated output are exact"
    );
    assert!(!run("1 + 1").partial);

    let mut env = MapEnv::default();
    env.values
        .insert("subtotal".into(), ScalarValue::Integer(5));
    env.partial_values.insert("subtotal".into());
    assert!(
        evaluate_detailed(&parse("subtotal * 2").unwrap(), &env)
            .unwrap()
            .partial
    );
}
