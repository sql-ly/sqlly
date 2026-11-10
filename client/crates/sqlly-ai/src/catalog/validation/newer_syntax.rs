//! Valid statements that other SQL tools flag as errors, each from a public
//! bug report. SQLly's parser accepts grammar it does not model rather than
//! guessing, so none of these may produce a finding: no recovery warning, no
//! unresolved table or column, against a catalog that has every table they
//! read.

use super::test_support::*;
use super::*;
use crate::catalog::wire::CatalogObject;
use sqlly_lang::sqlmodel::Dialect;

fn assert_clean(dialect: Dialect, objects: &[CatalogObject], sql: &str) {
    let store = store_with(objects);
    let options = SqlValidationOptions {
        dialect,
        allow_multiple_statements: true,
        ..SqlValidationOptions::default()
    };
    let result = validate_sql(&store, sql, &options).unwrap();
    assert!(result.issues.is_empty(), "{dialect:?}: {sql}\n{result:?}");
}

/// DataGrip DBE-25433, DBE-26419, DBE-19638, DBE-19512.
#[test]
fn postgres_ddl_and_time_zone_syntax() {
    let wines = [table("wines", &["winename", "stock"])];
    for sql in [
        "SELECT current_time AT LOCAL;",
        "SELECT now() AT LOCAL, stock FROM wines WHERE stock > 1",
        "CREATE TABLE foo (\n    bar TEXT STORAGE EXTENDED\n);",
        "CREATE PUBLICATION pub_name FOR TABLES IN SCHEMA my_schema;",
        "create type test_range_type as range (\n\tsubtype = time,\n    MULTIRANGE_TYPE_NAME = timemultirange\n);",
        "CREATE TYPE float8_range AS RANGE (subtype = float8, subtype_diff = float8mi);",
    ] {
        assert_clean(Dialect::Postgres, &wines, sql);
    }
}

/// DataGrip DBE-26807, DBE-23448: SQL/JSON query functions and JSON_TABLE,
/// including columns of the JSON_TABLE used bare in WHERE.
#[test]
fn postgres_sql_json_functions_and_json_table() {
    for sql in [
        "SELECT JSON_EXISTS(jsonb '{\"k\":[1,2,3]}', 'strict $.k[*] ? (@ > $x)' PASSING 2 AS x);",
        "SELECT JSON_QUERY(jsonb '{\"a\":\"[1,2]\"}', 'lax $.a' OMIT QUOTES);",
        "SELECT JSON_VALUE(jsonb '\"123.45\"', '$' RETURNING float);",
    ] {
        assert_clean(Dialect::Postgres, &[], sql);
    }
    let xml = [table("xmldata", &["hls", "_xml", "doc"])];
    for sql in [
        "SELECT\n    hls,\n    (xpath('normalize-space(//*[@id=\"views\"]/text())', _xml))[1]::text AS views,\n    video_object.*\nFROM\n    xmldata,\n    JSON_TABLE(\n        (xpath('normalize-space(/html/head/script[@type=\"application/ld+json\"])', _xml))[1]::text::jsonb,\n        '$'\n        COLUMNS (\n            name        text      PATH '$.name',\n            description text      PATH '$.description',\n            uploadDate  timestamp PATH '$.uploadDate',\n            duration    interval  PATH '$.duration',\n            interations int       PATH '$.interaction.uCount'\n        )\n    ) AS video_object",
        "SELECT name FROM xmldata, JSON_TABLE(doc, '$' COLUMNS (name text PATH '$.name')) AS jt WHERE name = 'x'",
        "SELECT * FROM xmldata x, JSON_TABLE(x.doc, '$[*]' COLUMNS (id FOR ORDINALITY, NESTED PATH '$.tags[*]' COLUMNS (tag text PATH '$'))) AS jt WHERE tag = 'a' AND id > 1",
    ] {
        assert_clean(Dialect::Postgres, &xml, sql);
    }
    assert_clean(
        Dialect::MySql,
        &xml,
        "SELECT jt.name FROM xmldata, JSON_TABLE(doc, '$[*]' COLUMNS (name VARCHAR(40) PATH '$.name')) AS jt WHERE name = 'x'",
    );
}

/// DataGrip DBE-26790: PostgreSQL 16 and 17 EXPLAIN options and EXPLAIN MERGE.
#[test]
fn postgres_explain_options_and_merge() {
    let objects = [
        table("pg_class", &["oid", "relname"]),
        table("wines", &["winename", "stock"]),
        table("new_wine_list", &["winename", "stock"]),
    ];
    for sql in [
        "EXPLAIN (MEMORY) SELECT 1;",
        "EXPLAIN (ANALYZE, SERIALIZE TEXT) SELECT;",
        "EXPLAIN (GENERIC_PLAN) SELECT * FROM pg_class WHERE oid = $1;",
        "EXPLAIN MERGE INTO wines w\n    USING new_wine_list s ON s.winename = w.winename\n    WHEN NOT MATCHED BY TARGET THEN\n        INSERT VALUES (s.winename, s.stock)\n    WHEN MATCHED AND w.stock <> s.stock THEN\n        UPDATE SET stock = s.stock\n    WHEN NOT MATCHED BY SOURCE THEN\n        DELETE;",
    ] {
        assert_clean(Dialect::Postgres, &objects, sql);
    }
}

/// Azure Data Studio #26268: an aggregated view with STRING_AGG … WITHIN GROUP.
#[test]
fn sql_server_view_with_ordered_string_agg() {
    let objects = [
        table("user_table", &["id", "username", "email", "enabled"]),
        table("user_role", &["user_id", "role_id"]),
        table("role_table", &["id", "name"]),
    ];
    assert_clean(
        Dialect::SqlServer,
        &objects,
        "CREATE OR ALTER VIEW v_users_with_roles AS\n  SELECT u.id, u.username, u.email, u.enabled, STRING_AGG(r.name, '|') WITHIN GROUP (ORDER BY r.name) AS roles FROM user_table u\n  LEFT JOIN user_role ur ON u.id = ur.user_id\n  LEFT JOIN role_table r ON ur.role_id = r.id\n  GROUP BY u.id, u.username, u.email, u.enabled",
    );
}

/// DBeaver #42372: built-in and user functions, in any case, are never
/// reported as unknown. HeidiSQL #2619: MariaDB's EXECUTE IMMEDIATE.
#[test]
fn mysql_builtin_functions_and_execute_immediate() {
    let wines = [table("wines", &["winename", "stock"])];
    for sql in [
        "select if(true, 1, 0), greatest(1,2), test.f() from dual;",
        "SELECT IF(stock > 1, 'y', 'n'), GREATEST(stock, 2), instr(winename, 'a') FROM wines WHERE stock > 1",
        "EXECUTE IMMEDIATE 'SELECT 1';",
        "EXECUTE IMMEDIATE CONCAT('SELECT ', 1);",
    ] {
        assert_clean(Dialect::MySql, &wines, sql);
    }
}

/// DBeaver #42337: window frames with UNBOUNDED PRECEDING, in every dialect.
#[test]
fn window_frames_with_unbounded_preceding() {
    let t = [table("t", &["id", "x"])];
    for dialect in Dialect::ALL {
        for sql in [
            "SELECT id, SUM(x) OVER (ORDER BY id ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW) AS running FROM t WHERE x > 0",
            "SELECT id, SUM(x) OVER (ORDER BY id ROWS UNBOUNDED PRECEDING) AS running FROM t",
            "SELECT id, AVG(x) OVER (PARTITION BY id ORDER BY x RANGE BETWEEN UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING) FROM t",
        ] {
            assert_clean(dialect, &t, sql);
        }
    }
}
