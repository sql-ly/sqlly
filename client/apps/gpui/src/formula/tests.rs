//! Expected values are what Excel returns for the same formula.

use super::*;

/// A tiny sheet: columns Name, Qty, Price, Day over three rows.
fn sheet() -> (Vec<&'static str>, Vec<Vec<Value>>) {
    (
        vec!["Name", "Qty", "Price", "Day", "Order Total"],
        vec![
            vec![
                Value::Text("apple".into()),
                Value::Number(2.0),
                Value::Number(1.5),
                Value::Number(serial_from_date(
                    NaiveDate::from_ymd_opt(2026, 1, 31).unwrap(),
                )),
                Value::Blank,
            ],
            vec![
                Value::Text("Pear".into()),
                Value::Number(5.0),
                Value::Number(0.5),
                Value::Number(serial_from_date(
                    NaiveDate::from_ymd_opt(2026, 2, 28).unwrap(),
                )),
                Value::Number(7.0),
            ],
            vec![
                Value::Text("fig".into()),
                Value::Blank,
                Value::Number(4.0),
                Value::Blank,
                Value::Text("".into()),
            ],
        ],
    )
}

fn eval_at(src: &str, row: i64) -> Value {
    let (cols, rows) = sheet();
    let formula = parse(src).unwrap_or_else(|e| panic!("{src}: {e}"));
    let cell = |name: &str, r: i64| -> Option<Value> {
        let c = cols.iter().position(|c| c.eq_ignore_ascii_case(name))?;
        Some(
            usize::try_from(r)
                .ok()
                .and_then(|r| rows.get(r))
                .map(|row| row[c].clone())
                .unwrap_or(Value::Blank),
        )
    };
    let between = |a: &str, b: &str| -> Option<Vec<String>> {
        let i = cols.iter().position(|c| c.eq_ignore_ascii_case(a))?;
        let j = cols.iter().position(|c| c.eq_ignore_ascii_case(b))?;
        Some(
            cols[i.min(j)..=i.max(j)]
                .iter()
                .map(|s| s.to_string())
                .collect(),
        )
    };
    let variable = |name: &str| match name {
        "region" => Some(Value::Text("West".into())),
        "rate" => Some(Value::Number(0.25)),
        _ => None,
    };
    let env = EvalEnv {
        row,
        cell: &cell,
        columns_between: &between,
        variable: &variable,
        now: serial_from_date(NaiveDate::from_ymd_opt(2026, 10, 23).unwrap()) + 0.5,
    };
    evaluate(&formula.expr, &env)
}

fn eval(src: &str) -> Value {
    eval_at(src, 1)
}

fn n(x: f64) -> Value {
    Value::Number(x)
}

fn t(s: &str) -> Value {
    Value::Text(s.into())
}

#[test]
fn precedence_and_associativity_match_excel() {
    assert_eq!(eval("=-2^2"), n(4.0));
    assert_eq!(eval("=2^3^2"), n(64.0));
    assert_eq!(eval("=1+2*3"), n(7.0));
    assert_eq!(eval("=(1+2)*3"), n(9.0));
    assert_eq!(eval("=50%"), n(0.5));
    assert_eq!(eval("=-50%*2"), n(-1.0));
    assert_eq!(eval("=1+2&3"), t("33"));
    assert_eq!(eval("=1+2=3"), Value::Bool(true));
    assert_eq!(eval("=10-4-3"), n(3.0));
    assert_eq!(eval("=12/2/3"), n(2.0));
}

#[test]
fn coercion_and_errors_match_excel() {
    assert_eq!(eval("=\"3\"+4"), n(7.0));
    assert_eq!(eval("=TRUE+1"), n(2.0));
    assert_eq!(eval("=\"x\"+1"), Value::Error(ErrorKind::Value));
    assert_eq!(eval("=1/0"), Value::Error(ErrorKind::Div0));
    assert_eq!(eval("=FOO(1)"), Value::Error(ErrorKind::Name));
    assert_eq!(eval("=!Missing"), Value::Error(ErrorKind::Ref));
    assert_eq!(eval("=@nope"), Value::Error(ErrorKind::Name));
    assert_eq!(eval("=#N/A"), Value::Error(ErrorKind::NA));
    assert_eq!(eval("=\"a\"=\"A\""), Value::Bool(true));
    assert_eq!(eval("=\"a\"<1"), Value::Bool(false));
    assert_eq!(eval("=1&TRUE"), t("1TRUE"));
    assert_eq!(eval("=0.1+0.2"), n(0.1 + 0.2));
    assert_eq!(eval("=(0.1+0.2)&\"\""), t("0.3"));
}

#[test]
fn references_read_this_row_earlier_rows_and_fixed_rows() {
    assert_eq!(eval("=!Qty*!Price"), n(2.5));
    assert_eq!(eval("=!Qty[1]"), n(2.0));
    assert_eq!(eval("=!Qty[-1]"), n(0.0), "a blank cell reads as 0");
    assert_eq!(eval("=!Qty[$1]+!Qty"), n(7.0));
    assert_eq!(
        eval_at("=!Qty[1]", 0),
        n(0.0),
        "before the first row is blank"
    );
    assert_eq!(eval("=!\"Order Total\"+1"), n(8.0));
    assert_eq!(eval("=!qty"), n(5.0), "names ignore case");
    assert_eq!(eval_at("=SUM(!Qty[2]:!Qty)", 2), n(7.0));
    assert_eq!(eval("=SUM(!Qty:!Price)"), n(5.5));
    assert_eq!(eval("=!Qty:!Price"), Value::Error(ErrorKind::Value));
}

#[test]
fn variables_read_declared_values() {
    assert_eq!(eval("=@region&\"-\"&!Name"), t("West-Pear"));
    assert_eq!(eval("=!Qty*(1+@rate)"), n(6.25));
}

#[test]
fn logical_and_information_functions() {
    assert_eq!(eval("=IF(!Qty>3,\"big\",\"small\")"), t("big"));
    assert_eq!(eval("=IF(FALSE,1)"), Value::Bool(false));
    assert_eq!(eval("=IFS(!Qty>9,\"a\",!Qty>4,\"b\")"), t("b"));
    assert_eq!(eval("=IFS(!Qty>9,\"a\")"), Value::Error(ErrorKind::NA));
    assert_eq!(eval("=IFERROR(1/0,\"none\")"), t("none"));
    assert_eq!(eval("=AND(TRUE,!Qty>1)"), Value::Bool(true));
    assert_eq!(eval("=OR(FALSE,0)"), Value::Bool(false));
    assert_eq!(eval("=XOR(TRUE,TRUE)"), Value::Bool(false));
    assert_eq!(eval("=SWITCH(!Name,\"pear\",1,\"fig\",2,9)"), n(1.0));
    assert_eq!(eval("=CHOOSE(2,\"a\",\"b\")"), t("b"));
    assert_eq!(eval_at("=COALESCE(!\"Order Total\",!Qty,0)", 2), n(0.0));
    assert_eq!(eval_at("=COALESCE(!\"Order Total\",!Price)", 2), n(4.0));
    assert_eq!(eval_at("=ISBLANK(!Qty)", 2), Value::Bool(true));
    assert_eq!(eval("=ISNUMBER(!Name)"), Value::Bool(false));
    assert_eq!(eval("=N(\"x\")"), n(0.0));
}

#[test]
fn math_functions() {
    assert_eq!(eval("=ROUND(2.675,2)"), n(2.68));
    assert_eq!(eval("=ROUND(-2.5,0)"), n(-3.0));
    assert_eq!(eval("=ROUND(1234.5,-2)"), n(1200.0));
    assert_eq!(eval("=ROUNDUP(3.21,1)"), n(3.3));
    assert_eq!(eval("=ROUNDDOWN(-3.29,1)"), n(-3.2));
    assert_eq!(eval("=INT(-4.5)"), n(-5.0));
    assert_eq!(eval("=MOD(-3,2)"), n(1.0));
    assert_eq!(eval("=CEILING(4.2,0.5)"), n(4.5));
    assert_eq!(eval("=FLOOR(4.7,2)"), n(4.0));
    assert_eq!(eval("=SQRT(-1)"), Value::Error(ErrorKind::Num));
    assert_eq!(eval("=POWER(2,10)"), n(1024.0));
    assert_eq!(eval("=AVERAGE(1,2,3,4)"), n(2.5));
    assert_eq!(eval("=MAX(!Qty:!Price,10)"), n(10.0));
    assert_eq!(eval_at("=COUNT(!Qty[2]:!Qty)", 2), n(2.0));
    assert_eq!(eval_at("=COUNTA(!Name[2]:!Name)", 2), n(3.0));
    assert_eq!(eval("=MEDIAN(3,1,2,10)"), n(2.5));
    assert_eq!(eval("=EVEN(1.5)"), n(2.0));
    assert_eq!(eval("=ODD(2)"), n(3.0));
}

#[test]
fn text_functions() {
    assert_eq!(eval("=LEFT(!Name,2)&RIGHT(!Name)"), t("Per"));
    assert_eq!(eval("=MID(\"spreadsheet\",7,5)"), t("sheet"));
    assert_eq!(eval("=LEN(\"héllo\")"), n(5.0));
    assert_eq!(eval("=UPPER(!Name)&LOWER(\"X\")"), t("PEARx"));
    assert_eq!(eval("=PROPER(\"o'neil smith\")"), t("O'Neil Smith"));
    assert_eq!(eval("=TRIM(\"  a   b \")"), t("a b"));
    assert_eq!(eval("=SUBSTITUTE(\"a-b-c\",\"-\",\"+\",2)"), t("a-b+c"));
    assert_eq!(eval("=REPLACE(\"abcdef\",2,3,\"X\")"), t("aXef"));
    assert_eq!(eval("=FIND(\"b\",\"abcb\",3)"), n(4.0));
    assert_eq!(eval("=SEARCH(\"B?\",\"aabcd\")"), n(3.0));
    assert_eq!(eval("=FIND(\"z\",\"abc\")"), Value::Error(ErrorKind::Value));
    assert_eq!(eval("=TEXTJOIN(\", \",TRUE,\"a\",\"\",\"b\")"), t("a, b"));
    assert_eq!(eval("=CONCAT(!Name[1]:!Name)"), t("applePear"));
    assert_eq!(eval("=VALUE(\"1,234.5\")"), n(1234.5));
    assert_eq!(eval("=TEXTBEFORE(\"a.b.c\",\".\",-1)"), t("a.b"));
    assert_eq!(eval("=TEXTAFTER(\"a.b.c\",\".\")"), t("b.c"));
    assert_eq!(eval("=REPT(\"ab\",3)"), t("ababab"));
    assert_eq!(eval("=EXACT(\"a\",\"A\")"), Value::Bool(false));
    assert_eq!(eval("=FIXED(1234.567,1)"), t("1,234.6"));
}

#[test]
fn text_format_codes() {
    assert_eq!(eval("=TEXT(1234.5,\"#,##0.00\")"), t("1,234.50"));
    assert_eq!(eval("=TEXT(0.256,\"0.0%\")"), t("25.6%"));
    assert_eq!(eval("=TEXT(5,\"000\")"), t("005"));
    assert_eq!(eval("=TEXT(-5,\"$#,##0\")"), t("-$5"));
    assert_eq!(eval("=TEXT(!Day,\"yyyy-mm-dd\")"), t("2026-02-28"));
    assert_eq!(eval("=TEXT(!Day,\"mmm d, yyyy\")"), t("Feb 28, 2026"));
    assert_eq!(eval("=TEXT(!Day,\"dddd\")"), t("Saturday"));
    assert_eq!(eval("=TEXT(TIME(14,5,0),\"h:mm AM/PM\")"), t("2:05 PM"));
    assert_eq!(eval("=TEXT(TIME(9,7,3),\"hh:mm:ss\")"), t("09:07:03"));
}

#[test]
fn date_functions() {
    let d = |y, m, dd| n(serial_from_date(NaiveDate::from_ymd_opt(y, m, dd).unwrap()));
    assert_eq!(eval("=DATE(2026,13,1)"), d(2027, 1, 1));
    assert_eq!(eval("=DATE(2026,3,0)"), d(2026, 2, 28));
    assert_eq!(eval("=YEAR(!Day)*100+MONTH(!Day)"), n(202602.0));
    assert_eq!(eval_at("=EDATE(!Day,1)", 0), d(2026, 2, 28));
    assert_eq!(eval_at("=EOMONTH(!Day,1)", 0), d(2026, 2, 28));
    assert_eq!(eval("=!Day-!Day[1]"), n(28.0));
    assert_eq!(eval("=DATEDIF(!Day[1],!Day,\"D\")"), n(28.0));
    assert_eq!(
        eval("=DATEDIF(DATE(2020,5,15),DATE(2026,2,10),\"Y\")"),
        n(5.0)
    );
    assert_eq!(
        eval("=DATEDIF(DATE(2020,5,15),DATE(2026,2,10),\"YM\")"),
        n(8.0)
    );
    assert_eq!(eval("=WEEKDAY(!Day)"), n(7.0));
    assert_eq!(eval("=WEEKDAY(!Day,2)"), n(6.0));
    assert_eq!(eval("=WEEKNUM(DATE(2026,1,4))"), n(2.0));
    assert_eq!(eval("=TODAY()"), d(2026, 10, 23));
    assert_eq!(eval("=HOUR(NOW())"), n(12.0));
    assert_eq!(
        eval("=NETWORKDAYS(DATE(2026,10,19),DATE(2026,10,25))"),
        n(5.0)
    );
    assert_eq!(eval("=WORKDAY(DATE(2026,10,23),1)"), d(2026, 10, 26));
    assert_eq!(eval("=DATEVALUE(\"2026-10-23\")"), d(2026, 10, 23));
    assert_eq!(eval("=DAYS(DATE(2026,3,1),DATE(2026,2,1))"), n(28.0));
}

#[test]
fn general_format_matches_excel() {
    assert_eq!(format_general(1.0), "1");
    assert_eq!(format_general(0.1 + 0.2), "0.3");
    assert_eq!(format_general(1.0 / 3.0), "0.333333333333333");
    assert_eq!(format_general(-12.5), "-12.5");
    assert_eq!(format_general(1e20), "1E+20");
    assert_eq!(format_general(1.5e-12), "1.5E-12");
}

#[test]
fn database_dates_read_as_serials() {
    let base = serial_from_date(NaiveDate::from_ymd_opt(2026, 10, 23).unwrap());
    assert_eq!(parse_datetime_text("2026-10-23"), Some(base));
    assert_eq!(parse_datetime_text("2026-10-23 12:00:00"), Some(base + 0.5));
    assert_eq!(
        parse_datetime_text("2026-10-23T06:00:00.000Z"),
        Some(base + 0.25)
    );
    assert_eq!(
        parse_datetime_text("2026-10-23 18:00:00 +02:00"),
        Some(base + 0.75)
    );
    assert_eq!(parse_datetime_text("12:00"), Some(0.5));
    assert_eq!(parse_datetime_text("not a date"), None);
}

#[test]
fn parse_errors_point_at_the_problem() {
    for (src, at) in [
        ("=1+", 3),
        ("=SUM(1,2", 8),
        ("=1 2", 3),
        ("=Qty*2", 1),
        ("=(1", 3),
        ("=!", 2),
        ("=!A[x]", 3),
        ("=\"open", 1),
        ("=", 1),
    ] {
        let e = parse(src).expect_err(src);
        assert_eq!(e.at, at, "{src}: {}", e.message);
    }
}

#[test]
fn reference_spans_cover_the_source_for_coloring() {
    let f = parse("=!Qty*!\"Order Total\"[1]+SUM(!Qty[2]:!Qty)").unwrap();
    let spans: Vec<&str> = f.refs.iter().map(|r| &f.source[r.start..r.end]).collect();
    assert_eq!(
        spans,
        vec!["!Qty", "!\"Order Total\"[1]", "!Qty[2]", "!Qty"]
    );
    assert_eq!(f.referenced_columns(), vec!["Qty", "Order Total"]);
    assert!(f.reads_other_rows());
    assert!(!parse("=!Qty*2").unwrap().reads_other_rows());
}

#[test]
fn excel_formulas_use_sheet_references() {
    let letters = |name: &str| match name.to_ascii_lowercase().as_str() {
        "qty" => Some("B".to_string()),
        "price" => Some("C".to_string()),
        _ => None,
    };
    let var = |name: &str| format!("sqlly_{name}");
    let ctx = ExcelRefContext {
        column_letter: &letters,
        first_data_row: 2,
        row_count: 3,
        row: 1,
        variable_name: &var,
    };
    let x = |src: &str| to_excel_formula(&parse(src).unwrap().expr, &ctx);
    assert_eq!(x("=!Qty*!Price").as_deref(), Some("B3*C3"));
    assert_eq!(x("=!Qty-!Qty[1]").as_deref(), Some("B3-B2"));
    assert_eq!(x("=!Qty/!Qty[$1]").as_deref(), Some("B3/B$2"));
    assert_eq!(x("=!Qty[5]").as_deref(), Some("B1048576"));
    assert_eq!(x("=(1+2)*3-(4-5)").as_deref(), Some("(1+2)*3-(4-5)"));
    assert_eq!(x("=2^(3^2)").as_deref(), Some("2^(3^2)"));
    assert_eq!(
        x("=SUM(!Qty[1]:!Qty)*@rate").as_deref(),
        Some("SUM(B2:B3)*sqlly_rate")
    );
    assert_eq!(
        x("=COALESCE(!Qty,0)").as_deref(),
        Some("IF((B3)&\"\"<>\"\",B3,0)")
    );
    assert_eq!(
        x("=IFS(TRUE,\"a\")").as_deref(),
        Some("_xlfn.IFS(TRUE,\"a\")")
    );
    assert_eq!(x("=!Nope"), None);
}

#[test]
fn every_listed_function_is_implemented() {
    for name in super::functions::FUNCTIONS {
        let v = eval(&format!("={name}()"));
        assert_ne!(v, Value::Error(ErrorKind::Name), "{name}");
    }
    assert_eq!(eval("=1+1").display(), "2");
}
