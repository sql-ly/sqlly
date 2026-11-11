use super::casing::{pascal_case_identifier, safe_to_unquote, title_case};
use super::segment::control_comment_state;
use super::*;
use crate::sqlformat::prefs::*;

/// Baseline test prefs: defaults minus the one-liner shortcut, so layout
/// assertions actually exercise layout. FROM tables and WHERE conditions are
/// pinned to the keyword line (the pre-2026-10-22 default) so the per-option
/// assertions below stay focused on the option they test; the new-line
/// default is covered by the `clause_body_placement_*` tests.
fn prefs() -> SqlFormattingPreferences {
    SqlFormattingPreferences {
        keep_short_queries_on_one_line: false,
        from_table_placement: ClauseBodyPlacement::SameLine,
        where_condition_placement: ClauseBodyPlacement::SameLine,
        ..Default::default()
    }
}

fn fmt(sql: &str, p: &SqlFormattingPreferences) -> String {
    format_sql(sql, p, None)
}

const CORPUS: &[&str] = &[
    "select 1",
    "select id, name from users where id = 1 and status = 'active' order by name desc",
    "SELECT u.id, u.name, count(*) AS order_count FROM users u LEFT JOIN orders o ON o.user_id = u.id AND o.deleted_at IS NULL GROUP BY u.id, u.name HAVING count(*) > 10 ORDER BY order_count DESC LIMIT 50",
    "with recent as (select id, created_at from orders where created_at > '2026-01-01'), totals as (select user_id, sum(amount) as total from order_items group by user_id) select r.id, t.total from recent r join totals t on t.user_id = r.id",
    "insert into audit_log (event, actor, created_at) values ('login', 'matt', now()), ('logout', 'sam', now())",
    "update users set name = 'Bob', updated_at = now() where id = 7",
    "delete from sessions where expires_at < now()",
    "create table users (id bigint not null primary key, email varchar(255) not null, display_name text, created_at timestamp default now(), constraint uq_email unique (email))",
    "select case when status = 'a' then 1 when status = 'b' then 2 else 0 end as code, id from t",
    "select * from a union all select * from b order by 1",
    "select id from t where x in (1, 2, 3) and y between 1 and 10 and z like 'a%'",
    "select id, -- trailing note\n  name /* block */ from t -- end\n",
    "-- leading comment\nselect 1;\n\nselect 2;",
    "select (select max(x) from t2 where t2.a = t1.a) as mx from t1",
    "merge into target t using source s on t.id = s.id when matched then update set v = s.v when not matched then insert (id, v) values (s.id, s.v)",
    "select \"Weird Name\", [bracket col], `tick` from x",
    "select a::text, b -> 'k', c || d from t where e @@ f",
    "select count(*), coalesce(a, b, c), substring(x, 1, 3) from t",
];

#[test]
fn empty_and_whitespace_inputs_pass_through() {
    let p = prefs();
    assert_eq!(fmt("", &p), "");
    assert_eq!(fmt("   \n  ", &p), "   \n  ");
}

#[test]
fn broken_sql_is_returned_unchanged() {
    let p = prefs();
    let broken = "select 'unterminated from t";
    assert_eq!(fmt(broken, &p), broken);
}

#[test]
fn default_layout_shapes_a_simple_query() {
    let p = prefs();
    let out = fmt(
        "select id, name from users where id = 1 and status = 'x'",
        &p,
    );
    assert_eq!(
        out,
        "SELECT\n    id,\n    name\nFROM users\nWHERE id = 1\n    AND status = 'x'"
    );
}

#[test]
fn corpus_is_idempotent_under_every_preset() {
    for preset in Preset::all() {
        let mut p = SqlFormattingPreferences::for_preset(*preset);
        for sql in CORPUS {
            let once = fmt(sql, &p);
            let twice = fmt(&once, &p);
            assert_eq!(
                once, twice,
                "not idempotent under {preset:?} for input: {sql}"
            );
        }
        // Also with the one-liner shortcut off.
        p.keep_short_queries_on_one_line = false;
        for sql in CORPUS {
            let once = fmt(sql, &p);
            let twice = fmt(&once, &p);
            assert_eq!(
                once, twice,
                "not idempotent (no-shortcut) under {preset:?} for input: {sql}"
            );
        }
    }
}

// The debug_assert inside format_sql turns any token corruption into a
// panic in tests, so this doubles as the token-safety sweep.
#[test]
fn corpus_survives_aggressive_option_combinations() {
    let mut variants: Vec<SqlFormattingPreferences> = Vec::new();
    for preset in Preset::all() {
        variants.push(SqlFormattingPreferences::for_preset(*preset));
    }
    variants.push(SqlFormattingPreferences {
        keyword_case: KeywordCase::TitleCase,
        identifier_case: IdentifierCase::PascalCase,
        identifier_quoting: IdentifierQuoting::AlwaysQuote,
        comma_style: CommaStyle::Leading,
        boolean_operator_placement: BooleanOperatorPlacement::LineTrailing,
        parenthesize_mixed_boolean_logic: true,
        spaces_around_operators: false,
        align_comparison_operators: true,
        join_layout: JoinLayout::Stacked,
        on_clause_placement: OnClausePlacement::OnNewLine,
        multi_condition_on: true,
        function_case: FunctionCase::Lowercase,
        space_before_function_parentheses: true,
        function_arg_style: FunctionArgStyle::Stacked,
        subquery_style: SubqueryStyle::Aligned,
        cte_style: crate::sqlformat::prefs::CteStyle::Inline,
        dml_style: DmlStyle::Expanded,
        values_style: ValuesStyle::Inline,
        insert_one_column_per_line: true,
        align_values_rows: true,
        align_set_operators: true,
        ddl_style: DdlStyle::Expanded,
        align_data_types: true,
        align_nullability: true,
        constraints_after_columns: true,
        comment_style: CommentStyle::Align,
        keep_inline_comments_inline: false,
        format_inside_block_comments: true,
        indent_style: IndentStyle::Tabs,
        indent_width_raw: 2,
        max_line_length_raw: 40,
        wrap_long_lines: true,
        keep_short_queries_on_one_line: false,
        case_expression_style: CaseExpressionStyle::Compact,
        in_list_style: InListStyle::Stacked,
        order_group_layout: OrderGroupLayout::Stacked,
        semicolon_style: SemicolonStyle::OwnLine,
        max_consecutive_blank_lines_raw: 0,
        align_column_aliases: true,
        ..Default::default()
    });
    let group = SqlFormattingPreferences {
        keyword_group_casing: KeywordGroupCasing {
            enabled: true,
            data_types: KeywordCase::Lowercase,
            functions: KeywordCase::TitleCase,
            keywords: KeywordCase::Uppercase,
            clauses: KeywordCase::Lowercase,
            operators: KeywordCase::TitleCase,
            joins: KeywordCase::Uppercase,
            set_operations: KeywordCase::Lowercase,
            dml: KeywordCase::Uppercase,
            ddl: KeywordCase::Lowercase,
            transaction: KeywordCase::Uppercase,
        },
        ..Default::default()
    };
    variants.push(group);

    for (vi, p) in variants.iter().enumerate() {
        for sql in CORPUS {
            let _ = fmt(sql, p); // debug_assert enforces safety
            let _ = vi;
        }
    }
}

#[test]
fn keyword_casing_variants() {
    let mut p = prefs();
    p.keyword_case = KeywordCase::Lowercase;
    assert_eq!(fmt("SELECT id FROM t", &p), "select\n    id\nfrom t");
    p.keyword_case = KeywordCase::Preserve;
    assert_eq!(fmt("SeLeCt id FrOm t", &p), "SeLeCt\n    id\nFrOm t");
    p.keyword_case = KeywordCase::MatchSurroundingStyle;
    assert_eq!(
        fmt("select id from T_TAB", &p),
        "select\n    id\nfrom T_TAB"
    );
    assert_eq!(
        fmt("SELECT id from T_TAB", &p),
        "SELECT\n    id\nFROM T_TAB"
    );
}

#[test]
fn keyword_group_casing_overrides() {
    let mut p = prefs();
    p.keyword_group_casing.enabled = true;
    p.keyword_group_casing.clauses = KeywordCase::Lowercase;
    p.keyword_group_casing.joins = KeywordCase::Uppercase;
    p.keyword_group_casing.operators = KeywordCase::Lowercase;
    let out = fmt("SELECT a FROM t JOIN u ON t.id = u.id WHERE a AND b", &p);
    assert!(out.contains("select"), "{out}");
    assert!(out.contains("JOIN"), "{out}");
    assert!(out.contains("and b"), "{out}");
}

#[test]
fn identifier_casing() {
    let mut p = prefs();
    p.identifier_case = IdentifierCase::Uppercase;
    assert!(fmt("select my_col from t", &p).contains("MY_COL"));
    p.identifier_case = IdentifierCase::PascalCase;
    assert!(fmt("select my_col from t", &p).contains("My_Col"));
    // Quoted identifiers never change case.
    p.identifier_case = IdentifierCase::Uppercase;
    assert!(fmt("select \"my_col\" from t", &p).contains("\"my_col\""));
}

#[test]
fn identifier_quoting_always_quote() {
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::AlwaysQuote;
    p.dialect = SqlDialect::Postgresql;
    let out = fmt("select col_a from tab", &p);
    assert!(out.contains("\"col_a\""), "{out}");
    assert!(out.contains("\"tab\""), "{out}");
}

#[test]
fn identifier_quoting_remove_unnecessary_is_case_safe() {
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::RemoveUnnecessaryQuotes;
    p.dialect = SqlDialect::Postgresql;
    // lowercase quoted → safe to unquote in PG
    assert!(fmt("select \"plain\" from t", &p).contains("plain"));
    assert!(!fmt("select \"plain\" from t", &p).contains("\"plain\""));
    // mixed case quoted → NOT safe in PG (case folding would change it)
    assert!(fmt("select \"MixedCase\" from t", &p).contains("\"MixedCase\""));
    // keyword-as-identifier stays quoted
    assert!(fmt("select \"select\" from t", &p).contains("\"select\""));
}

#[test]
fn quoted_reserved_words_keep_their_quotes_in_every_mode() {
    // `User` is not one of the lexer's parser keywords, but SQL Server
    // reserves it: `Shared.User` does not parse.
    for quoting in IdentifierQuoting::all() {
        let mut p = prefs();
        p.identifier_quoting = *quoting;
        p.dialect = SqlDialect::Tsql;
        let out = fmt(
            "select u.[Name] from Shared.[User] u join dbo.[Order] o on 1 = 1",
            &p,
        );
        assert!(out.contains("[User]"), "{quoting:?}: {out}");
        assert!(out.contains("[Order]"), "{quoting:?}: {out}");
    }
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::RemoveUnnecessaryQuotes;
    p.dialect = SqlDialect::Tsql;
    let out = fmt("select u.[Name] from Shared.[User] u", &p);
    assert!(out.contains("u.Name"), "{out}");
    p.dialect = SqlDialect::Postgresql;
    assert!(fmt("select \"user\".id from \"user\"", &p).contains("\"user\".id"));
}

#[test]
fn quote_only_when_required_quotes_qualified_reserved_words() {
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::QuoteOnlyWhenRequired;
    p.dialect = SqlDialect::Tsql;
    let out = fmt("select u.Name from Shared.User u", &p);
    assert!(out.contains("Shared.[User]"), "{out}");
    assert!(out.contains("u.Name"), "{out}");
    assert_eq!(fmt(&out, &p), out, "not idempotent:\n{out}");
    // A lone USER is the current-user function, not a name.
    assert!(fmt("select USER", &p).contains("USER"));
    assert!(!fmt("select USER", &p).contains("[USER]"));
    // Postgres folds the bare name, so the quoted spelling is the folded one.
    p.dialect = SqlDialect::Postgresql;
    assert!(fmt("select * from public.User", &p).contains("public.\"user\""));
    // Remove Unnecessary Quotes never adds quotes.
    p.identifier_quoting = IdentifierQuoting::RemoveUnnecessaryQuotes;
    p.dialect = SqlDialect::Tsql;
    assert!(fmt("select * from Shared.User", &p).contains("Shared.User"));
}

#[test]
fn always_quote_keeps_case_meaning_and_niladic_functions() {
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::AlwaysQuote;
    p.dialect = SqlDialect::Postgresql;
    // `Orders` unquoted is `orders` in Postgres; `"Orders"` would be another table.
    assert!(fmt("select * from Orders", &p).contains("\"orders\""));
    p.dialect = SqlDialect::Tsql;
    let out = fmt("select USER, x.User from Shared.User x", &p);
    assert!(out.contains("USER,"), "{out}");
    assert!(out.contains("[x].[User]"), "{out}");
    assert!(out.contains("[Shared].[User]"), "{out}");
}

#[test]
fn comma_style_leading() {
    let mut p = prefs();
    p.comma_style = CommaStyle::Leading;
    let out = fmt("select aaa, bbb, ccc from t", &p);
    assert_eq!(out, "SELECT\n    aaa\n    , bbb\n    , ccc\nFROM t");
}

#[test]
fn boolean_operator_trailing() {
    let mut p = prefs();
    p.boolean_operator_placement = BooleanOperatorPlacement::LineTrailing;
    let out = fmt("select a from t where x = 1 and y = 2", &p);
    assert!(out.contains("WHERE x = 1 AND\n"), "{out}");
}

#[test]
fn between_and_does_not_split_conditions() {
    let p = prefs();
    let out = fmt("select a from t where x between 1 and 10 and y = 2", &p);
    assert!(
        out.contains("WHERE x BETWEEN 1 AND 10\n    AND y = 2"),
        "{out}"
    );
}

#[test]
fn parenthesize_mixed_boolean_logic_groups_and_runs() {
    let mut p = prefs();
    p.parenthesize_mixed_boolean_logic = true;
    let out = fmt("select a from t where p = 1 and q = 2 or r = 3", &p);
    assert!(out.contains("(p = 1"), "{out}");
    assert!(out.contains("q = 2)"), "{out}");
}

#[test]
fn spaces_around_operators_off() {
    let mut p = prefs();
    p.spaces_around_operators = false;
    let out = fmt("select a from t where x = 1", &p);
    assert!(out.contains("x=1"), "{out}");
}

#[test]
fn join_layouts() {
    let mut p = prefs();
    let sql = "select a from t join u on t.id = u.id";
    p.join_layout = JoinLayout::Aligned;
    assert!(fmt(sql, &p).contains("\nJOIN u ON t.id = u.id"));
    p.join_layout = JoinLayout::Stacked;
    assert!(fmt(sql, &p).contains("\n    JOIN u ON t.id = u.id"));
}

#[test]
fn on_clause_new_line() {
    let mut p = prefs();
    p.on_clause_placement = OnClausePlacement::OnNewLine;
    let out = fmt("select a from t join u on t.id = u.id", &p);
    assert!(out.contains("JOIN u\n    ON t.id = u.id"), "{out}");
}

#[test]
fn multi_condition_on_stacks_conditions() {
    let mut p = prefs();
    p.multi_condition_on = true;
    let out = fmt("select a from t join u on t.id = u.id and t.k = u.k", &p);
    assert!(out.contains("ON t.id = u.id\n"), "{out}");
    assert!(out.contains("AND t.k = u.k"), "{out}");
}

#[test]
fn function_case_and_parens_space() {
    let mut p = prefs();
    p.function_case = FunctionCase::Lowercase;
    let out = fmt("select COUNT(*) from t", &p);
    assert!(out.contains("count(*)"), "{out}");
    p.space_before_function_parentheses = true;
    let out = fmt("select COUNT(*) from t", &p);
    assert!(out.contains("count (*)"), "{out}");
}

#[test]
fn function_args_stacked() {
    let mut p = prefs();
    p.function_arg_style = FunctionArgStyle::Stacked;
    let out = fmt("select coalesce(a, b, c) from t", &p);
    assert!(out.contains("COALESCE(\n"), "{out}");
    assert!(out.contains("    a,\n"), "{out}");
}

#[test]
fn in_list_styles() {
    let sql = "select a from t where x in (1, 2, 3)";
    let mut p = prefs();
    p.in_list_style = InListStyle::Inline;
    assert!(fmt(sql, &p).contains("IN (1, 2, 3)"));
    p.in_list_style = InListStyle::Stacked;
    let out = fmt(sql, &p);
    assert!(out.contains("IN (\n"), "{out}");
    assert!(out.contains("1,\n"), "{out}");
}

#[test]
fn in_subquery_is_not_an_in_list() {
    let p = prefs();
    let out = fmt("select a from t where x in (select id from u)", &p);
    assert!(out.contains("SELECT"), "{out}");
}

#[test]
fn case_expression_stacked_and_compact() {
    let sql = "select case when a = 1 then 'x' else 'y' end from t";
    let mut p = prefs();
    p.case_expression_style = CaseExpressionStyle::Stacked;
    let out = fmt(sql, &p);
    assert!(out.contains("CASE\n"), "{out}");
    assert!(out.contains("    WHEN a = 1 THEN 'x'\n"), "{out}");
    assert!(out.contains("    ELSE 'y'\n"), "{out}");
    p.case_expression_style = CaseExpressionStyle::Compact;
    let out = fmt(sql, &p);
    assert!(
        out.contains("CASE WHEN a = 1 THEN 'x' ELSE 'y' END"),
        "{out}"
    );
}

#[test]
fn order_group_layout_inline() {
    let mut p = prefs();
    p.order_group_layout = OrderGroupLayout::Inline;
    let out = fmt("select a, b from t group by a, b order by a, b", &p);
    assert!(out.contains("GROUP BY a, b"), "{out}");
    assert!(out.contains("ORDER BY a, b"), "{out}");
}

#[test]
fn semicolon_own_line() {
    let mut p = prefs();
    p.semicolon_style = SemicolonStyle::OwnLine;
    let out = fmt("select a from t;", &p);
    assert!(out.ends_with("\n;"), "{out}");
}

#[test]
fn blank_lines_between_statements_clamped() {
    let mut p = prefs();
    p.max_consecutive_blank_lines_raw = 1;
    let out = fmt("select 1;\n\n\n\n\nselect 2;", &p);
    assert!(out.contains("SELECT\n    1;\n\nSELECT\n    2;"), "{out}");
    p.max_consecutive_blank_lines_raw = 0;
    let out = fmt("select 1;\n\n\nselect 2;", &p);
    assert!(out.contains("1;\nSELECT"), "{out}");
}

#[test]
fn comments_are_never_dropped() {
    let p = prefs();
    let sql = "-- top\nselect a, -- inline\n b /* mid */ from t -- tail";
    let out = fmt(sql, &p);
    for needle in ["-- top", "-- inline", "/* mid */", "-- tail"] {
        assert!(out.contains(needle), "lost {needle:?} in: {out}");
    }
}

#[test]
fn control_comments_disable_formatting() {
    let p = prefs();
    let sql = "-- formatter:off\nselect   1,2   from t;\n-- formatter:on\nselect 3 from u;";
    let out = fmt(sql, &p);
    // First statement (after the off toggle) is verbatim.
    assert!(out.contains("select   1,2   from t"), "{out}");
    // Second statement is formatted again.
    assert!(out.contains("SELECT"), "{out}");
}

#[test]
fn control_comment_prefix_is_configurable() {
    let mut p = prefs();
    p.control_comment_prefix = "sqlfmt".into();
    let sql = "-- sqlfmt:off\nselect   1   from t;\n-- sqlfmt:on\nselect 2 from u;";
    let out = fmt(sql, &p);
    assert!(out.contains("select   1   from t"), "{out}");
    // The default prefix no longer toggles.
    let sql2 = "-- formatter:off\nselect   1   from t;";
    let out2 = fmt(sql2, &p);
    assert!(out2.contains("SELECT"), "{out2}");
}

#[test]
fn respect_control_comments_off_formats_everything() {
    let mut p = prefs();
    p.respect_control_comments = false;
    let sql = "-- formatter:off\nselect   1   from t";
    let out = fmt(sql, &p);
    assert!(out.contains("SELECT"), "{out}");
}

#[test]
fn short_query_stays_on_one_line() {
    let mut p = prefs();
    p.keep_short_queries_on_one_line = true;
    p.short_query_max_length_raw = 80;
    assert_eq!(
        fmt("select id, name from users", &p),
        "SELECT id, name FROM users"
    );
    // Over the limit → normal layout.
    p.short_query_max_length_raw = 40;
    let long = "select id, name, email, phone from customer_records";
    assert!(fmt(long, &p).contains('\n'));
}

#[test]
fn select_one_line_when_short_respects_threshold() {
    let mut p = prefs();
    p.select_column_layout = SelectListStyle::OneLineWhenShort;
    p.inline_column_threshold_raw = 3;
    let out = fmt("select a, b from t where x = 1", &p);
    assert!(out.contains("SELECT a, b"), "{out}");
    let out = fmt("select a, b, c, d from t where x = 1", &p);
    assert!(out.contains("SELECT\n    a"), "{out}");
}

#[test]
fn values_stacked_and_aligned() {
    let mut p = prefs();
    p.values_style = ValuesStyle::Stacked;
    p.align_values_rows = true;
    let out = fmt("insert into t (a, b) values (1, 'xx'), (100, 'y')", &p);
    assert!(out.contains("(1,   'xx')"), "{out}");
    assert!(out.contains("(100, 'y')"), "{out}");
}

#[test]
fn insert_one_column_per_line() {
    let mut p = prefs();
    p.insert_one_column_per_line = true;
    let out = fmt("insert into t (aaa, bbb) values (1, 2)", &p);
    assert!(out.contains("(\n    aaa,\n    bbb\n)"), "{out}");
}

#[test]
fn update_set_stacked_with_aligned_assignments() {
    let mut p = prefs();
    p.align_set_operators = true;
    let out = fmt(
        "update users set name = 'x', updated_at_ts = now() where id = 1",
        &p,
    );
    assert!(out.contains("name          = 'x'"), "{out}");
    assert!(out.contains("updated_at_ts = NOW()"), "{out}");
}

#[test]
fn create_table_standard_layout() {
    let p = prefs();
    let out = fmt(
        "create table t (id bigint not null, name varchar(50), primary key (id))",
        &p,
    );
    assert!(out.contains("CREATE TABLE t (\n"), "{out}");
    assert!(out.contains("    id BIGINT NOT NULL,\n"), "{out}");
    assert!(out.contains("    name VARCHAR(50),\n"), "{out}");
    assert!(out.contains("\n)"), "{out}");
}

#[test]
fn create_table_aligned_types_and_nullability() {
    let mut p = prefs();
    p.align_data_types = true;
    p.align_nullability = true;
    let out = fmt(
        "create table t (id bigint not null, display_name varchar(50) null)",
        &p,
    );
    assert!(out.contains("id           BIGINT"), "{out}");
    assert!(out.contains("display_name VARCHAR(50)"), "{out}");
}

#[test]
fn ddl_compact_when_fits() {
    let mut p = prefs();
    p.ddl_style = DdlStyle::Compact;
    p.max_line_length_raw = 200;
    let out = fmt("create table t (id int, v text)", &p);
    assert!(out.contains("(id INT, v TEXT)"), "{out}");
}

#[test]
fn cte_layout_default() {
    let p = prefs();
    let out = fmt(
        "with x as (select id from t where id > 5) select * from x",
        &p,
    );
    assert!(out.starts_with("WITH\n    x AS ("), "{out}");
    assert!(out.contains("\nSELECT"), "{out}");
}

#[test]
fn no_new_line_after_with() {
    let mut p = prefs();
    p.new_line_after_with = false;
    let out = fmt(
        "with x as (select id from t where id > 5) select * from x",
        &p,
    );
    assert!(out.starts_with("WITH x AS ("), "{out}");
}

#[test]
fn subquery_indented_and_expanded() {
    let mut p = prefs();
    let sql = "select a from (select b from t where b > 1) d where a < 5";
    p.subquery_style = SubqueryStyle::Indented;
    let out = fmt(sql, &p);
    assert!(out.contains("FROM (\n"), "{out}");
    assert!(out.contains("    SELECT\n"), "{out}");
    p.indent_nested_queries = false;
    let out2 = fmt(sql, &p);
    assert!(out2.contains("FROM (\nSELECT"), "{out2}");
}

#[test]
fn indent_styles() {
    let mut p = prefs();
    p.indent_style = IndentStyle::Tabs;
    let out = fmt("select a, b from t", &p);
    assert!(out.contains("\n\ta,"), "{out:?}");
    p.indent_style = IndentStyle::Spaces;
    p.indent_width_raw = 2;
    p.continuation_indent_size_raw = 2;
    let out = fmt("select a, b from t", &p);
    assert!(out.contains("\n  a,"), "{out:?}");
}

#[test]
fn editor_indent_override() {
    let mut p = prefs();
    p.use_editor_indentation = true;
    let out = format_sql(
        "select a, b from t",
        &p,
        Some(EditorIndent {
            style: IndentStyle::Spaces,
            width: 8,
        }),
    );
    assert!(out.contains("\n        a,"), "{out:?}");
}

#[test]
fn wrap_long_select_list() {
    let mut p = prefs();
    p.select_column_layout = SelectListStyle::Wrap;
    p.max_line_length_raw = 40;
    let out = fmt(
        "select column_one, column_two, column_three, column_four from t",
        &p,
    );
    assert!(out.lines().all(|l| l.chars().count() <= 40), "{out}");
    assert!(out.lines().count() > 2, "{out}");
}

#[test]
fn blank_line_before_major_clauses() {
    let mut p = prefs();
    p.blank_line_before_major_clauses = true;
    let out = fmt("select a from t where a = 1", &p);
    assert!(out.contains("a\n\nFROM t\n\nWHERE"), "{out}");
}

#[test]
fn set_operations_on_their_own_lines() {
    let p = prefs();
    let out = fmt("select a from t union all select b from u", &p);
    assert!(out.contains("\nUNION ALL\n"), "{out}");
}

#[test]
fn tsql_brackets_and_params_survive() {
    let p = prefs();
    let out = fmt("select [odd name], @param from [dbo].[Table1]", &p);
    assert!(out.contains("[odd name]"), "{out}");
    assert!(out.contains("@param"), "{out}");
    assert!(out.contains("[dbo].[Table1]"), "{out}");
}

#[test]
fn postgres_casts_and_dollar_params_survive() {
    let p = prefs();
    let out = fmt("select a::text, $1, e.x from t e", &p);
    assert!(out.contains("a::TEXT") || out.contains("a::text"), "{out}");
    assert!(out.contains("$1"), "{out}");
}

#[test]
fn preserve_existing_preset_keeps_user_breaks() {
    let p = SqlFormattingPreferences::for_preset(Preset::PreserveExisting);
    let sql = "SELECT a,\n       b\nFROM t\nWHERE x = 1";
    let out = fmt(sql, &p);
    // Keywords keep their case and clause structure survives.
    assert!(out.contains("SELECT"), "{out}");
    assert!(out.contains("FROM t"), "{out}");
}

#[test]
fn helper_title_case() {
    assert_eq!(title_case("select"), "Select");
    assert_eq!(title_case("SELECT"), "Select");
}

#[test]
fn helper_pascal_case_identifier() {
    assert_eq!(pascal_case_identifier("user_id"), "User_Id");
    assert_eq!(pascal_case_identifier("name"), "Name");
}

#[test]
fn helper_control_comment_state() {
    assert_eq!(
        control_comment_state("-- formatter:off", "formatter"),
        Some(false)
    );
    assert_eq!(
        control_comment_state("--formatter: on", "formatter"),
        Some(true)
    );
    assert_eq!(
        control_comment_state("/* formatter:off */", "formatter"),
        Some(false)
    );
    assert_eq!(control_comment_state("-- note", "formatter"), None);
    assert_eq!(
        control_comment_state("-- formatter:offbeat", "formatter"),
        None
    );
}

#[test]
fn helper_safe_to_unquote() {
    assert!(safe_to_unquote("plain", Dialect::Postgres));
    assert!(!safe_to_unquote("Mixed", Dialect::Postgres));
    assert!(!safe_to_unquote("select", Dialect::Postgres));
    assert!(!safe_to_unquote("user", Dialect::Postgres));
    assert!(!safe_to_unquote("User", Dialect::SqlServer));
    assert!(safe_to_unquote("Mixed", Dialect::SqlServer));
    assert!(!safe_to_unquote("has space", Dialect::SqlServer));
    assert!(safe_to_unquote("UPPER", Dialect::Oracle));
    assert!(!safe_to_unquote("lower_mixed_Case", Dialect::Oracle));
}

#[test]
fn helper_detect_dialect() {
    assert_eq!(detect_dialect("select [x] from t").0, Dialect::SqlServer);
    assert_eq!(detect_dialect("select @p from t").0, Dialect::SqlServer);
    assert_eq!(detect_dialect("select `x` from t").0, Dialect::MySql);
    assert_eq!(detect_dialect("select x from t").0, Dialect::Postgres);
}

#[test]
fn multiple_statements_keep_their_order_and_semicolons() {
    let p = prefs();
    let out = fmt("select 1; select 2; select 3;", &p);
    assert_eq!(out.matches(';').count(), 3);
    let p1 = out.find('1').unwrap();
    let p2 = out.find('2').unwrap();
    let p3 = out.find('3').unwrap();
    assert!(p1 < p2 && p2 < p3);
}

// ---------------------------------------------------------------------------
// T-SQL `GO` batch separators
// ---------------------------------------------------------------------------

#[test]
fn go_separator_stays_on_its_own_line() {
    let p = prefs();
    let out = fmt("select id from t\nGO\nselect name from u\nGO", &p);
    assert_eq!(
        out, "SELECT\n    id\nFROM t\nGO\nSELECT\n    name\nFROM u\nGO",
        "{out}"
    );
}

#[test]
fn short_go_scripts_do_not_collapse_on_default_settings() {
    let p = SqlFormattingPreferences::default();
    let out = fmt("select 1\ngo\nselect 2\ngo", &p);
    assert!(out.lines().filter(|l| *l == "GO").count() == 2, "{out}");
    assert!(!out.contains("1 GO") && !out.contains("1 go"), "{out}");
    // A single batch still gets the short-query one-liner — GO stays below it.
    let out = fmt("select id from t\nGO", &p);
    assert_eq!(out, "SELECT id FROM t\nGO", "{out}");
}

#[test]
fn go_with_repeat_count_and_semicolon() {
    let p = prefs();
    let out = fmt("insert into t (a) values (1);\ngo 5\nselect 1;", &p);
    assert!(out.contains("VALUES\n    (1);\nGO 5\nSELECT"), "{out}");
}

#[test]
fn go_is_case_insensitive_and_keeps_trailing_comment() {
    let p = prefs();
    let out = fmt("select 1 from t\nGo -- end of batch\nselect 2 from u", &p);
    assert!(out.contains("FROM t\nGO -- end of batch\nSELECT"), "{out}");
}

#[test]
fn go_preserves_blank_lines_between_batches() {
    let p = prefs();
    let out = fmt("select 1 from t\nGO\n\nselect 2 from u", &p);
    assert!(out.contains("FROM t\nGO\n\nSELECT"), "{out}");
}

#[test]
fn go_column_on_its_own_line_is_not_a_separator() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Tsql,
        ..prefs()
    };
    // `go` as a select-list column, alone on its line, mid-statement.
    for sql in [
        "select a,\ngo\nfrom t",
        "select\ngo\nfrom t",
        "select a\n, go\n, b from t",
        "select a, go\nfrom t",
        "select a\ngo\nfrom t",
        "select x from t where y =\ngo\nand z = 1",
        "select count(\ngo\n) from t",
    ] {
        // Laid out exactly like the same statement with a different column.
        let out = fmt(sql, &p).to_ascii_lowercase();
        let control = fmt(&sql.replace("go", "gq"), &p)
            .to_ascii_lowercase()
            .replace("gq", "go");
        assert_eq!(out, control, "`go` treated as a separator in {sql:?}");
    }
}

#[test]
fn go_inside_identifiers_strings_and_comments_is_not_a_separator() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Tsql,
        ..prefs()
    };
    let out = fmt(
        "select 'x\ngo\ny', [go], go_live\n-- go\n/*\ngo\n*/\nfrom t",
        &p,
    );
    assert!(!out.lines().any(|l| l == "GO"), "{out}");
    assert!(out.contains("'x\ngo\ny'"), "{out}");
}

#[test]
fn go_ends_a_formatter_off_statement_verbatim() {
    let p = prefs();
    let sql = "-- formatter:off\nselect   1   from t\ngo\n-- formatter:on\nselect 2 from u";
    let out = fmt(sql, &p);
    // The protected batch and its separator are kept exactly as typed…
    assert!(out.contains("select   1   from t\ngo\n"), "{out}");
    // …and the next batch is formatted again.
    assert!(out.ends_with("SELECT\n    2\nFROM u"), "{out}");
}

#[test]
fn go_is_only_a_separator_in_tsql() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Postgresql,
        ..prefs()
    };
    let out = fmt("select 1 from t\ngo\nselect 2 from u", &p);
    assert!(!out.lines().any(|l| l.eq_ignore_ascii_case("go")), "{out}");
}

#[test]
fn go_separator_detects_tsql() {
    assert_eq!(
        detect_dialect("select x from t\nGO\nselect y from u").0,
        Dialect::SqlServer
    );
    assert_eq!(detect_dialect("select a,\ngo\nfrom t").0, Dialect::Postgres);
}

#[test]
fn go_scripts_are_idempotent_under_every_preset() {
    let scripts = [
        "select 1\ngo\nselect 2\ngo",
        "create procedure p as begin select 1; end\nGO\nexec p;\nGO 3",
        "select 1;\nGO\n\n-- next batch\nselect 2 from t\nGO -- done",
        "GO\nselect 1",
        "-- formatter:off\nselect   1   from t\ngo\n-- formatter:on\nselect 2 from u",
    ];
    for preset in Preset::all() {
        let p = SqlFormattingPreferences::for_preset(*preset);
        for sql in scripts {
            let once = fmt(sql, &p);
            assert_eq!(
                once,
                fmt(&once, &p),
                "not idempotent under {preset:?}: {sql}"
            );
            assert!(
                once.lines().any(|l| l.trim().eq_ignore_ascii_case("go")
                    || l.trim().to_ascii_lowercase().starts_with("go ")),
                "GO lost its line under {preset:?}: {once}"
            );
        }
    }
}

// -- cte_style -----------------------------------------------------------------

const TWO_CTES: &str = "with recent as (select id, created_at from orders where created_at > '2026-01-01'), totals as (select user_id, sum(amount) as total from order_items group by user_id) select r.id, t.total from recent r join totals t on t.user_id = r.id";

#[test]
fn cte_style_inline_puts_each_cte_body_on_one_line() {
    let mut p = prefs();
    p.cte_style = CteStyle::Inline;
    assert_eq!(
        fmt(TWO_CTES, &p),
        "WITH\n    recent AS (SELECT id, created_at FROM orders WHERE created_at > '2026-01-01'),\n    totals AS (SELECT user_id, SUM(amount) AS total FROM order_items GROUP BY user_id)\nSELECT\n    r.id,\n    t.total\nFROM recent r\nJOIN totals t ON t.user_id = r.id"
    );
    p.new_line_after_with = false;
    p.keyword_case = KeywordCase::Lowercase;
    let out = fmt(TWO_CTES, &p);
    assert!(
        out.starts_with("with recent as (select id, created_at from orders where created_at > '2026-01-01'),\ntotals as (select "),
        "{out}"
    );
}

#[test]
fn cte_style_inline_falls_back_to_stacked_when_too_long() {
    let mut p = prefs();
    p.cte_style = CteStyle::Inline;
    p.max_line_length_raw = 60;
    let stacked = {
        let mut q = p.clone();
        q.cte_style = CteStyle::Stacked;
        fmt(TWO_CTES, &q)
    };
    assert_eq!(fmt(TWO_CTES, &p), stacked);
    assert!(
        stacked.contains("recent AS (\n        SELECT\n"),
        "{stacked}"
    );
}

#[test]
fn cte_style_inline_keeps_line_comments_stacked() {
    let mut p = prefs();
    p.cte_style = CteStyle::Inline;
    let out = fmt("with x as (select 1 -- one\n) select * from x", &p);
    assert!(out.contains("x AS (\n"), "{out}");
    assert!(out.contains("-- one"), "{out}");
}

#[test]
fn cte_style_stacked_is_the_default_layout() {
    let mut p = prefs();
    p.cte_style = CteStyle::Stacked;
    assert_eq!(fmt(TWO_CTES, &p), fmt(TWO_CTES, &prefs()));
    assert!(
        fmt(TWO_CTES, &p).starts_with("WITH\n    recent AS (\n        SELECT\n            id,"),
        "{}",
        fmt(TWO_CTES, &p)
    );
}

// -- dml_style -----------------------------------------------------------------

const DML_INSERT: &str = "insert into audit_log (event, actor, created_at) values ('login', 'matt', now()), ('logout', 'sam', now())";
const DML_UPDATE: &str = "update users set name = 'Bob', updated_at = now() where id = 7";
const DML_DELETE: &str = "delete from sessions where expires_at < now()";

#[test]
fn dml_style_standard_puts_each_clause_on_a_new_line() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Standard;
    assert_eq!(
        fmt(DML_INSERT, &p),
        "INSERT INTO audit_log (event, actor, created_at)\nVALUES\n    ('login', 'matt', NOW()),\n    ('logout', 'sam', NOW())"
    );
    assert_eq!(
        fmt(DML_UPDATE, &p),
        "UPDATE users\nSET name = 'Bob',\n    updated_at = NOW()\nWHERE id = 7"
    );
    assert_eq!(
        fmt(DML_DELETE, &p),
        "DELETE\nFROM sessions\nWHERE expires_at < NOW()"
    );
}

#[test]
fn dml_style_compact_fits_statements_on_one_line() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Compact;
    assert_eq!(
        fmt(DML_INSERT, &p),
        "INSERT INTO audit_log (event, actor, created_at) VALUES ('login', 'matt', NOW()), ('logout', 'sam', NOW())"
    );
    assert_eq!(
        fmt(&format!("{DML_UPDATE};"), &p),
        "UPDATE users SET name = 'Bob', updated_at = NOW() WHERE id = 7;"
    );
    assert_eq!(
        fmt(DML_DELETE, &p),
        "DELETE FROM sessions WHERE expires_at < NOW()"
    );
    // SELECT is not DML: unaffected.
    assert_eq!(fmt("select a from t", &p), "SELECT\n    a\nFROM t");
}

#[test]
fn dml_style_compact_falls_back_when_too_long_or_commented() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Compact;
    p.max_line_length_raw = 50;
    let mut standard = p.clone();
    standard.dml_style = DmlStyle::Standard;
    assert_eq!(fmt(DML_INSERT, &p), fmt(DML_INSERT, &standard));
    p.max_line_length_raw = 120;
    let commented = "update t set a = 1 -- why\nwhere id = 2";
    assert_eq!(fmt(commented, &p), fmt(commented, &standard));
}

#[test]
fn dml_style_expanded_puts_each_column_and_value_on_its_own_line() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Expanded;
    assert_eq!(
        fmt(DML_INSERT, &p),
        "INSERT INTO audit_log (\n    event,\n    actor,\n    created_at\n)\nVALUES\n    (\n        'login',\n        'matt',\n        NOW()\n    ),\n    (\n        'logout',\n        'sam',\n        NOW()\n    )"
    );
    assert_eq!(
        fmt(DML_UPDATE, &p),
        "UPDATE users\nSET\n    name = 'Bob',\n    updated_at = NOW()\nWHERE id = 7"
    );
    // Single-value tuples / single columns stay put.
    assert_eq!(
        fmt("insert into t (a) values (1)", &p),
        "INSERT INTO t (a)\nVALUES\n    (1)"
    );
}

#[test]
fn dml_style_expanded_opts_dml_out_of_the_short_query_one_liner() {
    let mut p = SqlFormattingPreferences {
        dml_style: DmlStyle::Expanded,
        ..Default::default()
    };
    assert!(p.keep_short_queries_on_one_line);
    let out = fmt("insert into t (a, b) values (1, 2)", &p);
    assert_eq!(
        out,
        "INSERT INTO t (\n    a,\n    b\n)\nVALUES\n    (\n        1,\n        2\n    )"
    );
    // Non-DML short queries still collapse.
    assert_eq!(fmt("select a from t", &p), "SELECT a FROM t");
    p.dml_style = DmlStyle::Standard;
    assert_eq!(
        fmt("insert into t (a, b) values (1, 2)", &p),
        "INSERT INTO t (a, b) VALUES (1, 2)"
    );
}

// -- multiline_index_columns -----------------------------------------------------

const CREATE_INDEX: &str = "create index ix_users_email on users (email, created_at desc) include (name, status) where active = 1";
const TABLE_WITH_KEYS: &str = "create table t (id int, name text, primary key (id, name), unique key uk (name, id), index ix (name, id), foreign key (id) references u (id))";

#[test]
fn index_column_lists_are_identifiers_not_calls() {
    let p = prefs();
    assert_eq!(
        fmt(CREATE_INDEX, &p),
        "CREATE INDEX ix_users_email ON users (email, created_at DESC) INCLUDE (name, status)\nWHERE active = 1"
    );
    assert_eq!(
        fmt("create unique index ix on dbo.t (a asc, b)", &p),
        "CREATE UNIQUE INDEX ix ON dbo.t (a ASC, b)"
    );
}

#[test]
fn multiline_index_columns_off_keeps_key_columns_inline() {
    let mut p = prefs();
    p.multiline_index_columns = false;
    let out = fmt(TABLE_WITH_KEYS, &p);
    assert!(out.contains("    PRIMARY KEY (id, name),\n"), "{out}");
    assert!(out.contains("    UNIQUE KEY uk (name, id),\n"), "{out}");
    assert!(out.contains("    INDEX ix (name, id),\n"), "{out}");
}

#[test]
fn multiline_index_columns_on_stacks_key_and_include_columns() {
    let mut p = prefs();
    p.multiline_index_columns = true;
    assert_eq!(
        fmt(CREATE_INDEX, &p),
        "CREATE INDEX ix_users_email ON users (\n    email,\n    created_at DESC\n) INCLUDE (\n    name,\n    status\n)\nWHERE active = 1"
    );
    assert_eq!(
        fmt("create index ix on t using btree (a, b)", &p),
        "CREATE INDEX ix ON t USING btree (\n    a,\n    b\n)"
    );
    // A single key column stays inline.
    assert_eq!(
        fmt("create index ix on t (a)", &p),
        "CREATE INDEX ix ON t (a)"
    );
    let out = fmt(TABLE_WITH_KEYS, &p);
    assert!(
        out.contains("    PRIMARY KEY (\n        id,\n        name\n    ),\n"),
        "{out}"
    );
    assert!(
        out.contains("    UNIQUE KEY uk (\n        name,\n        id\n    ),\n"),
        "{out}"
    );
    assert!(
        out.contains("    INDEX ix (\n        name,\n        id\n    ),\n"),
        "{out}"
    );
    // FOREIGN KEY columns are not index key columns.
    assert!(out.contains("    FOREIGN KEY (id) REFERENCES"), "{out}");
    // Nor are MySQL index hints.
    assert!(
        fmt("select a from t use index (ix_a, ix_b)", &p).contains("(ix_a, ix_b)"),
        "hint list must stay inline"
    );
}

// -- move_trailing_comments_with_expression ----------------------------------------

const TRAILING_COMMENTS: &str =
    "select a, -- note a\n b -- note b\n from t where x = 1 -- cond\n and y = 2";

#[test]
fn move_trailing_comments_off_keeps_todays_placement() {
    let mut p = prefs();
    p.move_trailing_comments_with_expression = false;
    assert_eq!(
        fmt(TRAILING_COMMENTS, &p),
        "SELECT\n    a,\n    -- note a\n    b\n-- note b\nFROM t\nWHERE x = 1 -- cond\n    AND y = 2"
    );
}

#[test]
fn move_trailing_comments_on_keeps_comments_with_their_expression() {
    let mut p = prefs();
    p.move_trailing_comments_with_expression = true;
    assert_eq!(
        fmt(TRAILING_COMMENTS, &p),
        "SELECT\n    a, -- note a\n    b -- note b\nFROM t\nWHERE x = 1 -- cond\n    AND y = 2"
    );
    assert_eq!(
        fmt("select -- cols\n a, b from t", &p),
        "SELECT -- cols\n    a,\n    b\nFROM t"
    );
    assert_eq!(
        fmt("update t set a = 1, -- first\n b = 2 where id = 3", &p),
        "UPDATE t\nSET a = 1, -- first\n    b = 2\nWHERE id = 3"
    );
    assert_eq!(
        fmt("insert into t (a, -- col a\n b) values (1, 2)", &p),
        "INSERT INTO t (a, -- col a\n    b)\nVALUES\n    (1, 2)"
    );
    assert_eq!(
        fmt("select 1; -- one\nselect 2;", &p),
        "SELECT\n    1; -- one\nSELECT\n    2;"
    );
}

#[test]
fn move_trailing_comments_align_with_comment_style_align() {
    let mut p = prefs();
    p.move_trailing_comments_with_expression = true;
    p.comment_style = CommentStyle::Align;
    let out = fmt(TRAILING_COMMENTS, &p);
    assert!(
        out.contains("    a, -- note a\n    b  -- note b\n"),
        "{out}"
    );
}

#[test]
fn move_trailing_comments_defers_to_keep_inline_comments_off() {
    let mut p = prefs();
    p.move_trailing_comments_with_expression = true;
    p.keep_inline_comments_inline = false;
    let mut off = p.clone();
    off.move_trailing_comments_with_expression = false;
    assert_eq!(fmt(TRAILING_COMMENTS, &p), fmt(TRAILING_COMMENTS, &off));
}

#[test]
fn new_layout_options_are_idempotent_and_safe() {
    let extra = [
        TWO_CTES,
        DML_INSERT,
        DML_UPDATE,
        DML_DELETE,
        CREATE_INDEX,
        TABLE_WITH_KEYS,
        TRAILING_COMMENTS,
        "select -- cols\n a, b from t",
        "update t set a = 1, -- first\n b = 2 where id = 3",
        "insert into t (a, -- col a\n b) values (1, 2)",
        "select 1; -- one\nselect 2;",
        "with x as ( -- body\n select 1) select * from x",
        "create unique nonclustered index ix on dbo.t (a asc, b) include (c, d) with (fillfactor = 80)",
        INSERT_INTO_KEY,
        CREATE_TABLE_INDEX,
        INDEX_HINT,
        MERGE_UPDATE_SET,
        QUALIFIED_KEYWORD_COLUMNS,
    ];
    let mut variants = Vec::new();
    for base in [prefs(), SqlFormattingPreferences::default()] {
        for cte_style in CteStyle::all() {
            for dml_style in DmlStyle::all() {
                for flag in [false, true] {
                    variants.push(SqlFormattingPreferences {
                        cte_style: *cte_style,
                        dml_style: *dml_style,
                        multiline_index_columns: flag,
                        move_trailing_comments_with_expression: flag,
                        ..base.clone()
                    });
                }
            }
        }
    }
    for preset in Preset::all() {
        variants.push(SqlFormattingPreferences::for_preset(*preset));
    }
    for p in &variants {
        for sql in CORPUS.iter().chain(extra.iter()) {
            let once = fmt(sql, p);
            let twice = fmt(&once, p);
            assert_eq!(
                once, twice,
                "not idempotent (cte {:?}, dml {:?}, index/comments {}) for input: {sql}",
                p.cte_style, p.dml_style, p.multiline_index_columns
            );
        }
    }
}

// -- tables named like index words --------------------------------------------

const INSERT_INTO_KEY: &str = "insert into key (a, b) values (1, 2)";
const CREATE_TABLE_INDEX: &str = "create table index (a int not null, b text)";

#[test]
fn a_table_named_key_or_index_keeps_insert_and_ddl_treatment() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Expanded;
    // `INTO key (…)` is the insert column list (expanded one per line), not
    // an index key list (kept inline while multi-line index columns is off).
    p.multiline_index_columns = false;
    let out = fmt(INSERT_INTO_KEY, &p);
    assert!(
        out.ends_with(" (\n    a,\n    b\n)\nVALUES\n    (\n        1,\n        2\n    )"),
        "{out}"
    );
    let out = fmt("insert into index (a, b) values (1, 2)", &p);
    assert!(out.contains(" (\n    a,\n    b\n)\nVALUES"), "{out}");
    let out = fmt("insert into dbo.key (a, b) values (1, 2)", &p);
    assert!(out.contains("dbo.key (\n    a,\n    b\n)\nVALUES"), "{out}");
    // `TABLE index (…)` is a DDL body: types are cased, one column per line.
    let out = fmt(CREATE_TABLE_INDEX, &p);
    assert!(
        out.ends_with(" (\n    a INT NOT NULL,\n    b TEXT\n)"),
        "{out}"
    );
    let out = fmt("create table key (a int not null, b text)", &p);
    assert!(
        out.ends_with(" (\n    a INT NOT NULL,\n    b TEXT\n)"),
        "{out}"
    );
    // Real key lists are untouched by the exclusion.
    p.multiline_index_columns = true;
    let out = fmt(TABLE_WITH_KEYS, &p);
    assert!(
        out.contains("    INDEX ix (\n        name,\n        id\n    ),\n"),
        "{out}"
    );
    assert!(
        out.contains("    PRIMARY KEY (\n        id,\n        name\n    ),\n"),
        "{out}"
    );
}

// -- T-SQL index hints ------------------------------------------------------------

const INDEX_HINT: &str = "select a from t with (index (ix_a, ix_b)) where a = 1";

#[test]
fn tsql_index_hints_are_not_index_column_lists() {
    let mut p = prefs();
    p.multiline_index_columns = true;
    let out = fmt(INDEX_HINT, &p);
    assert!(out.contains("WITH (INDEX (ix_a, ix_b))"), "{out}");
    let out = fmt("select a from t with (nolock, index (ix_a, ix_b))", &p);
    assert!(out.contains("INDEX (ix_a, ix_b)"), "{out}");
}

// -- MERGE … UPDATE SET under Expanded DML ----------------------------------------

const MERGE_UPDATE_SET: &str = "merge into target t using source s on t.id = s.id when matched then update set a = s.a, b = s.b when not matched then insert (id, a) values (s.id, s.a)";

#[test]
fn dml_style_expanded_stacks_merge_update_set_assignments() {
    let mut p = prefs();
    p.dml_style = DmlStyle::Expanded;
    let out = fmt(MERGE_UPDATE_SET, &p);
    assert!(
        out.contains("THEN UPDATE SET\n    a = s.a,\n    b = s.b\nWHEN"),
        "{out}"
    );
    assert_eq!(fmt(&out, &p), out, "not idempotent:\n{out}");
    // Standard keeps the assignments on the WHEN line.
    p.dml_style = DmlStyle::Standard;
    let out = fmt(MERGE_UPDATE_SET, &p);
    assert!(
        out.contains("THEN UPDATE SET a = s.a, b = s.b\nWHEN"),
        "{out}"
    );
    // A single assignment stays put even when expanded.
    p.dml_style = DmlStyle::Expanded;
    let out = fmt(
        "merge into t using s on t.id = s.id when matched then update set a = s.a",
        &p,
    );
    assert!(out.contains("UPDATE SET a = s.a"), "{out}");
}

// -- keyword-named columns behind a qualifier -------------------------------------

const QUALIFIED_KEYWORD_COLUMNS: &str =
    "select o.type, t.key, s.index, t.*, 1.5, dbo.fn(x) from sys.objects o";

#[test]
fn qualified_keyword_named_columns_are_identifiers() {
    let p = prefs();
    assert_eq!(
        fmt("select o.type from sys.objects o", &p),
        "SELECT\n    o.type\nFROM sys.objects o"
    );
    assert_eq!(
        fmt("select t.key, s.index from t join s on s.id = t.id", &p),
        "SELECT\n    t.key,\n    s.index\nFROM t\nJOIN s ON s.id = t.id"
    );
    // Lower-casing keywords must not touch them either.
    let mut lower = prefs();
    lower.keyword_case = KeywordCase::Lowercase;
    assert_eq!(
        fmt("SELECT o.TYPE FROM sys.objects o", &lower),
        "select\n    o.TYPE\nfrom sys.objects o"
    );
    // Unchanged: star, numeric literal, schema-qualified call.
    assert_eq!(
        fmt("select t.*, 1.5, dbo.fn(x) from t", &p),
        "SELECT\n    t.*,\n    1.5,\n    dbo.FN(x)\nFROM t"
    );
    let out = fmt(QUALIFIED_KEYWORD_COLUMNS, &p);
    assert_eq!(fmt(&out, &p), out, "not idempotent:\n{out}");
}

#[test]
fn qualified_keyword_named_columns_quote_under_always_quote() {
    let mut p = prefs();
    p.identifier_quoting = IdentifierQuoting::AlwaysQuote;
    p.dialect = SqlDialect::Postgresql;
    assert_eq!(
        fmt("select o.type from sys.objects o", &p),
        "SELECT\n    \"o\".\"type\"\nFROM \"sys\".\"objects\" \"o\""
    );
    let out = fmt("select t.key, s.index, t.*, 1.5, dbo.fn(x) from t", &p);
    assert!(out.contains("\"t\".\"key\""), "{out}");
    assert!(out.contains("\"s\".\"index\""), "{out}");
    assert!(out.contains("\"t\".*"), "{out}");
    assert!(out.contains("1.5"), "{out}");
    // The schema-qualified call keeps function casing, as before.
    assert!(out.contains("\"dbo\".FN("), "{out}");
    assert_eq!(fmt(&out, &p), out, "not idempotent:\n{out}");
}

// -- FROM table / WHERE condition placement --------------------------------

const CAGE_QUERY: &str = "SELECT\n    CASE st.CurrencyId\n        WHEN 1 THEN '$'\n        ELSE 'C$'\n    END AS Currency,\n    st.CurrencyValue\nFROM Cage.SimpleTransaction AS st\nWHERE st.CreatedDate BETWEEN '10/01/2026' AND '10/31/2026'\n    AND st.TransactionTypeId = 35";

const CAGE_SELECT: &str = "SELECT\n    CASE st.CurrencyId\n        WHEN 1 THEN '$'\n        ELSE 'C$'\n    END AS Currency,\n    st.CurrencyValue\n";

fn placement(from: ClauseBodyPlacement, cond: ClauseBodyPlacement) -> SqlFormattingPreferences {
    SqlFormattingPreferences {
        from_table_placement: from,
        where_condition_placement: cond,
        ..Default::default()
    }
}

#[test]
fn clause_body_placement_defaults_to_new_line_indented() {
    let d = SqlFormattingPreferences::default();
    assert_eq!(d.from_table_placement, ClauseBodyPlacement::NewLineIndented);
    assert_eq!(
        d.where_condition_placement,
        ClauseBodyPlacement::NewLineIndented
    );
    let out = fmt(CAGE_QUERY, &d);
    assert_eq!(
        out,
        format!(
            "{CAGE_SELECT}FROM\n    Cage.SimpleTransaction AS st\nWHERE\n    st.CreatedDate BETWEEN '10/01/2026' AND '10/31/2026'\n    AND st.TransactionTypeId = 35"
        )
    );
    assert_eq!(fmt(&out, &d), out, "idempotent");
}

#[test]
fn clause_body_placement_same_line_keeps_table_and_condition_on_keyword_line() {
    use ClauseBodyPlacement::*;
    let p = placement(SameLine, SameLine);
    let out = fmt(CAGE_QUERY, &p);
    assert_eq!(out, CAGE_QUERY);
    assert_eq!(fmt(&out, &p), out, "idempotent");
}

#[test]
fn clause_body_placement_from_and_where_are_independent() {
    use ClauseBodyPlacement::*;
    let p = placement(SameLine, NewLineIndented);
    let out = fmt(CAGE_QUERY, &p);
    assert_eq!(
        out,
        format!(
            "{CAGE_SELECT}FROM Cage.SimpleTransaction AS st\nWHERE\n    st.CreatedDate BETWEEN '10/01/2026' AND '10/31/2026'\n    AND st.TransactionTypeId = 35"
        )
    );
    assert_eq!(fmt(&out, &p), out, "idempotent");

    let p = placement(NewLineIndented, SameLine);
    let out = fmt(CAGE_QUERY, &p);
    assert_eq!(
        out,
        format!(
            "{CAGE_SELECT}FROM\n    Cage.SimpleTransaction AS st\nWHERE st.CreatedDate BETWEEN '10/01/2026' AND '10/31/2026'\n    AND st.TransactionTypeId = 35"
        )
    );
    assert_eq!(fmt(&out, &p), out, "idempotent");
}

const JOIN_QUERY: &str = "select o.id, c.name from Sales.Orders as o inner join Sales.Customers as c on c.id = o.customer_id left join Sales.Regions r on r.id = c.region_id where o.total > 100 and o.status = 'open' group by o.id, c.name having count(*) > 1";

#[test]
fn clause_body_placement_new_line_indents_joins_with_the_first_table() {
    use ClauseBodyPlacement::*;
    let p = placement(NewLineIndented, NewLineIndented);
    let out = fmt(JOIN_QUERY, &p);
    assert_eq!(
        out,
        "SELECT\n    o.id,\n    c.name\nFROM\n    Sales.Orders AS o\n    INNER JOIN Sales.Customers AS c ON c.id = o.customer_id\n    LEFT JOIN Sales.Regions r ON r.id = c.region_id\nWHERE\n    o.total > 100\n    AND o.status = 'open'\nGROUP BY\n    o.id,\n    c.name\nHAVING\n    COUNT(*) > 1"
    );
    assert_eq!(fmt(&out, &p), out, "idempotent");

    // ON on its own line indents one step from its JOIN, not from FROM.
    let p = SqlFormattingPreferences {
        on_clause_placement: OnClausePlacement::OnNewLine,
        ..placement(NewLineIndented, NewLineIndented)
    };
    let out = fmt(JOIN_QUERY, &p);
    assert!(
        out.contains(
            "FROM\n    Sales.Orders AS o\n    INNER JOIN Sales.Customers AS c\n        ON c.id = o.customer_id\n    LEFT JOIN"
        ),
        "{out}"
    );
    assert_eq!(fmt(&out, &p), out, "idempotent");
}

#[test]
fn clause_body_placement_same_line_keeps_joins_aligned_with_from() {
    use ClauseBodyPlacement::*;
    let p = placement(SameLine, SameLine);
    let out = fmt(JOIN_QUERY, &p);
    assert_eq!(
        out,
        "SELECT\n    o.id,\n    c.name\nFROM Sales.Orders AS o\nINNER JOIN Sales.Customers AS c ON c.id = o.customer_id\nLEFT JOIN Sales.Regions r ON r.id = c.region_id\nWHERE o.total > 100\n    AND o.status = 'open'\nGROUP BY\n    o.id,\n    c.name\nHAVING COUNT(*) > 1"
    );
}

#[test]
fn clause_body_placement_keeps_a_trailing_keyword_comment_on_the_keyword_line() {
    let p = prefs_with_defaults_placement();
    let out = fmt(
        "select * from -- src\n t where -- filter\n a = 1 and b = 2",
        &p,
    );
    assert_eq!(
        out,
        "SELECT\n    *\nFROM -- src\n    t\nWHERE -- filter\n    a = 1\n    AND b = 2"
    );
    assert_eq!(fmt(&out, &p), out, "idempotent");
}

fn prefs_with_defaults_placement() -> SqlFormattingPreferences {
    SqlFormattingPreferences {
        keep_short_queries_on_one_line: false,
        ..Default::default()
    }
}

#[test]
fn compact_and_preserve_presets_keep_clause_bodies_on_the_keyword_line() {
    for preset in [Preset::Compact, Preset::PreserveExisting] {
        let p = SqlFormattingPreferences::for_preset(preset);
        assert_eq!(p.from_table_placement, ClauseBodyPlacement::SameLine);
        assert_eq!(p.where_condition_placement, ClauseBodyPlacement::SameLine);
    }
}

// ---------------------------------------------------------------------------
// format_sql_report: why the input came back unchanged
// ---------------------------------------------------------------------------

#[test]
fn report_matches_format_sql_and_names_the_dialect() {
    let p = prefs();
    for sql in CORPUS {
        let report = format_sql_report(sql, &p, None);
        assert_eq!(report.output, fmt(sql, &p), "{sql}");
        // A decline always means the input came back verbatim.
        if report.declined.is_some() {
            assert_eq!(report.output, *sql);
        }
    }
    // The corpus's mixed-quote line (backticks outside MySQL) is the one
    // that does not lex under the auto-detected dialect.
    let declined = CORPUS
        .iter()
        .filter(|sql| format_sql_report(sql, &p, None).declined.is_some())
        .count();
    assert_eq!(declined, 1);
    let tsql = SqlFormattingPreferences {
        dialect: SqlDialect::Tsql,
        ..prefs()
    };
    assert_eq!(
        format_sql_report("select 1", &tsql, None).dialect,
        Dialect::SqlServer
    );
    // AutoDetect reports what it detected.
    assert_eq!(
        format_sql_report("select `a` from t", &p, None).dialect,
        Dialect::MySql
    );
    assert_eq!(
        format_sql_report("select [a] from t", &p, None).dialect,
        Dialect::SqlServer
    );
}

#[test]
fn report_explains_a_lex_error_with_its_offset() {
    let p = prefs();
    let sql = "select a,\n  'unterminated from t";
    let report = format_sql_report(sql, &p, None);
    assert_eq!(report.output, sql);
    assert_eq!(
        report.declined,
        Some(FormatDecline::LexError {
            kind: crate::sqllex::LexErrorKind::UnterminatedString,
            offset: sql.find('\'').unwrap(),
        })
    );
    let report = format_sql_report("select 1 /* open", &p, None);
    assert_eq!(
        report.declined,
        Some(FormatDecline::LexError {
            kind: crate::sqllex::LexErrorKind::UnterminatedBlockComment,
            offset: 9,
        })
    );
}

#[test]
fn report_flags_empty_input() {
    let p = prefs();
    for sql in ["", "  \n "] {
        let report = format_sql_report(sql, &p, None);
        assert_eq!(report.declined, Some(FormatDecline::Empty), "{sql:?}");
        assert_eq!(report.output, sql);
    }
}

#[test]
fn report_on_already_formatted_sql_is_not_a_decline() {
    let p = prefs();
    let once = fmt(CORPUS[2], &p);
    let report = format_sql_report(&once, &p, None);
    assert_eq!(report.output, once);
    assert_eq!(report.declined, None);
}

// ---------------------------------------------------------------------------
// Dialect syntax the formatter must format, not decline, and never re-space
// into different SQL.
// ---------------------------------------------------------------------------

const DIALECT_CORPUS: &[(SqlDialect, &str)] = &[
    (
        SqlDialect::Tsql,
        "select c.id, count(*) as n into #recent from dbo.Orders o join ##shared s on s.id = o.id where o.d >= @since group by c.id;\nselect * from #recent with (nolock)\ngo\ndrop table if exists #recent\ngo",
    ),
    (
        SqlDialect::Tsql,
        "merge t using s on t.id = s.id when matched then update set t.v = s.v when not matched then insert (id) values (s.id) output $action;",
    ),
    (
        SqlDialect::Postgresql,
        "select payload->>'kind', payload#>>'{a,b}', tags[1], tags[2:3] from events where tags @> array['x']::text[] and payload ? 'k' and payload ?| array['a','b'] and x !~* 'y' and a||/b > 1",
    ),
    (
        SqlDialect::Postgresql,
        "select B'101', X'1F', U&'d\\0061t', E'a\\nb', $1, :psql_var from t where d > now() - interval '7 days' order by d nulls last",
    ),
    (
        SqlDialect::Mysql,
        "set @rank := 0;\nselect @rank := @rank + 1 as r, j->>'$.a', a <=> b, b'0101', x'ff', _utf8mb4'x', @@sql_mode from t where y>=-1;",
    ),
    (
        SqlDialect::Mysql,
        "create user bob@localhost identified by 'pw'",
    ),
    (
        SqlDialect::Sqlite,
        "create table if not exists notes (id integer primary key autoincrement, body text);\nselect x'ff', ?1, j->>'$.a', a == b from notes",
    ),
    (
        SqlDialect::Oracle,
        "declare v employees.salary%type; begin select salary into v from employees@hq where id = :id; if sql%rowcount = 1 then v := v**2; end if; end;\n/\nselect n'x', q'[it's]' from dual\n/",
    ),
    (
        SqlDialect::Snowflake,
        "select o.src:customer.name::string as c, f.value:sku::string from raw o, lateral flatten(input => o.src:items) f qualify row_number() over (partition by o.id order by o.ts desc) = 1",
    ),
    (
        SqlDialect::Bigquery,
        "select arr[offset(0)], [1, 2, 3] as xs, r'\\d+' from `p.d.t` where x = @param qualify count(*) over (partition by y) > 1 # note\n",
    ),
];

#[test]
fn dialect_corpus_formats_instead_of_declining() {
    for (dialect, sql) in DIALECT_CORPUS {
        let p = SqlFormattingPreferences {
            dialect: *dialect,
            ..prefs()
        };
        let report = format_sql_report(sql, &p, None);
        assert_eq!(report.declined, None, "{dialect:?} declined: {sql}");
        assert_ne!(report.output, *sql, "{dialect:?} left unchanged: {sql}");
    }
}

#[test]
fn dialect_corpus_is_idempotent_and_safe_under_every_preset() {
    let mut variants: Vec<SqlFormattingPreferences> = Preset::all()
        .iter()
        .map(|p| SqlFormattingPreferences::for_preset(*p))
        .collect();
    variants.push(SqlFormattingPreferences {
        spaces_around_operators: false,
        identifier_quoting: IdentifierQuoting::AlwaysQuote,
        identifier_case: IdentifierCase::Uppercase,
        space_before_function_parentheses: true,
        function_arg_style: FunctionArgStyle::Stacked,
        max_line_length_raw: 40,
        keep_short_queries_on_one_line: false,
        ..Default::default()
    });
    variants.push(SqlFormattingPreferences {
        identifier_quoting: IdentifierQuoting::RemoveUnnecessaryQuotes,
        keyword_case: KeywordCase::Lowercase,
        ..Default::default()
    });
    for base in &variants {
        for (dialect, sql) in DIALECT_CORPUS {
            let p = SqlFormattingPreferences {
                dialect: *dialect,
                ..base.clone()
            };
            // The debug_assert inside format_sql_report turns any token
            // corruption into a panic.
            let once = fmt(sql, &p);
            let twice = fmt(&once, &p);
            assert_eq!(
                once, twice,
                "not idempotent under {:?} for {sql}",
                base.preset
            );
        }
    }
}

/// Touching pairs that read differently with a space between them stay
/// touching, whatever the spacing options say.
#[test]
fn glued_syntax_is_never_re_spaced() {
    let cases: &[(SqlDialect, &str, &[&str])] = &[
        (
            SqlDialect::Mysql,
            "select j->>'$.a', a<=>b, b'01', x'ff', _utf8mb4'x' from t",
            &["->>", "<=>", "b'01'", "x'ff'", "_utf8mb4'x'"],
        ),
        (
            SqlDialect::Mysql,
            "create user bob@localhost",
            &["bob@localhost"],
        ),
        (
            SqlDialect::Sqlite,
            "select j->>'$.a', a==b, x'ff', ?1 from t",
            &["->>", "==", "x'ff'", "?1"],
        ),
        (
            SqlDialect::Postgresql,
            "select a#>>'{x}', b@>c, tags[1], 'x'::text[], U&'d' from t",
            &["#>>", "@>", "tags[1]", "::TEXT[]", "U&'d'"],
        ),
        (
            SqlDialect::Snowflake,
            "select src:customer.name::string from t",
            &["src:customer.name::"],
        ),
        (
            SqlDialect::Oracle,
            "select * from orders@hq where v = w**2",
            &["orders@hq", "**"],
        ),
        (
            SqlDialect::Oracle,
            "declare v emp.sal%type; begin null; end;",
            &["emp.sal%type"],
        ),
        (
            SqlDialect::Bigquery,
            "select \"\"\"a b\"\"\", '''c''' from t",
            &["\"\"\"a b\"\"\"", "'''c'''"],
        ),
        (
            SqlDialect::Tsql,
            "select * from t cross apply f(t.id) x",
            &["CROSS APPLY F(t.id) x"],
        ),
    ];
    for spaces in [true, false] {
        for (dialect, sql, keep) in cases {
            let p = SqlFormattingPreferences {
                dialect: *dialect,
                spaces_around_operators: spaces,
                ..prefs()
            };
            let out = fmt(sql, &p);
            for k in *keep {
                assert!(out.contains(k), "{dialect:?}: {k:?} missing from\n{out}");
            }
        }
    }
}

#[test]
fn trailing_sign_after_an_operator_keeps_its_space() {
    for dialect in [SqlDialect::Postgresql, SqlDialect::Mysql, SqlDialect::Tsql] {
        let p = SqlFormattingPreferences { dialect, ..prefs() };
        let out = fmt("select a from t where x>=-1 and y<>-2", &p);
        assert!(
            out.contains("x >= -1") && out.contains("y <> -2"),
            "{dialect:?}: {out}"
        );
    }
}

#[test]
fn oracle_slash_lines_stay_on_their_own_line() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Oracle,
        ..prefs()
    };
    let out = fmt(
        "begin null; end;\n/\ngrant select on t to app\n/\nselect 1 from dual",
        &p,
    );
    let slashes = out.lines().filter(|l| *l == "/").count();
    assert_eq!(slashes, 2, "{out}");
    assert!(out.ends_with("SELECT\n    1\nFROM DUAL"), "{out}");
    // Outside Oracle a lone `/` is just division.
    let pg = SqlFormattingPreferences {
        dialect: SqlDialect::Postgresql,
        ..prefs()
    };
    assert!(!fmt("select a\n/\nb from t", &pg).lines().any(|l| l == "/"));
}

#[test]
fn create_table_if_not_exists_keeps_its_name_and_body_layout() {
    for dialect in [
        SqlDialect::Sqlite,
        SqlDialect::Mysql,
        SqlDialect::Postgresql,
    ] {
        let p = SqlFormattingPreferences { dialect, ..prefs() };
        let out = fmt(
            "create table if not exists notes (id int primary key, body text not null)",
            &p,
        );
        assert!(
            out.starts_with("CREATE TABLE IF NOT EXISTS notes (\n"),
            "{dialect:?}: {out}"
        );
        assert!(
            out.contains("\n    body TEXT NOT NULL"),
            "{dialect:?}: {out}"
        );
    }
}

#[test]
fn context_keywords_take_keyword_casing() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Snowflake,
        ..prefs()
    };
    let out = fmt(
        "select row_number() over (partition by a order by b) as rn, listagg(x, ',') within group (order by x) from t where d > now() - interval '1 day' qualify rn = 1",
        &p,
    );
    for kw in [
        "OVER (PARTITION BY",
        "WITHIN GROUP",
        "INTERVAL '1 day'",
        "\nQUALIFY",
    ] {
        assert!(out.contains(kw), "{kw:?} missing from\n{out}");
    }
    let merge = SqlFormattingPreferences {
        dialect: SqlDialect::Tsql,
        ..prefs()
    };
    let out = fmt(
        "merge t using s on t.id = s.id when not matched then insert (id) values (s.id);",
        &merge,
    );
    assert!(
        out.contains("USING s") && out.contains("WHEN NOT MATCHED"),
        "{out}"
    );
    // A column that only shares the name keeps its own casing.
    let out = fmt("select qualify, partition, over from t", &p);
    assert!(
        out.contains("qualify") && out.contains("partition") && out.contains("over"),
        "{out}"
    );
}
