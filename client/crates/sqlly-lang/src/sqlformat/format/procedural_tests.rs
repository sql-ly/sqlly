//! Layout of routine bodies, procedural blocks and DDL statements.

use super::*;
use crate::sqlformat::prefs::*;

fn layout(dialect: SqlDialect, sql: &str) -> String {
    let p = SqlFormattingPreferences {
        dialect,
        ..SqlFormattingPreferences::for_preset(Preset::Default)
    };
    let report = format_sql_report(sql, &p, None);
    assert_eq!(report.declined, None, "declined: {sql}");
    assert_eq!(
        format_sql_report(&report.output, &p, None).output,
        report.output,
        "not idempotent: {sql}"
    );
    report.output
}

#[track_caller]
fn assert_layout(dialect: SqlDialect, sql: &str, expected: &str) {
    // The expectations are multi-line literals: a CRLF checkout of this file
    // must not change what they say.
    assert_eq!(layout(dialect, sql), expected.replace("\r\n", "\n"));
}

#[test]
fn tsql_procedure_header_and_body_are_laid_out() {
    assert_layout(
        SqlDialect::Tsql,
        "create procedure dbo.usp_GetOrders @CustomerId int, @From datetime = null, @Total money output as begin set nocount on; select o.Id from dbo.Orders o where o.CustomerId = @CustomerId; if @@rowcount = 0 begin print 'none' return end end",
        r#"CREATE PROCEDURE dbo.usp_GetOrders
    @CustomerId INT,
    @From DATETIME = NULL,
    @Total MONEY OUTPUT
AS
BEGIN
    SET NOCOUNT ON;
    SELECT
        o.Id
    FROM
        dbo.Orders o
    WHERE
        o.CustomerId = @CustomerId;
    IF @@rowcount = 0
    BEGIN
        PRINT 'none'
        RETURN
    END
END"#,
    );
}

#[test]
fn tsql_function_keeps_returns_and_options_on_their_own_lines() {
    assert_layout(
        SqlDialect::Tsql,
        "create function dbo.f (@a int) returns int with schemabinding, returns null on null input as begin return (@a * 2) end",
        r#"CREATE FUNCTION dbo.f(@a INT)
RETURNS INT
WITH SCHEMABINDING, RETURNS NULL ON NULL INPUT
AS
BEGIN
    RETURN (@a * 2)
END"#,
    );
}

#[test]
fn tsql_trigger_header_is_not_read_as_a_query() {
    assert_layout(
        SqlDialect::Tsql,
        "create trigger trg on t for insert, update, delete not for replication as if update(col) begin raiserror('no', 16, 1) rollback end",
        r#"CREATE TRIGGER trg
ON t
FOR INSERT, UPDATE, DELETE NOT FOR REPLICATION
AS
IF UPDATE (col)
BEGIN
    raiserror('no', 16, 1)
    ROLLBACK
END"#,
    );
}

#[test]
fn tsql_if_else_chains_indent_their_branches() {
    assert_layout(
        SqlDialect::Tsql,
        "if @a = 1 begin select 1 end else if @a = 2 begin select 2 end else begin select 3 end\nselect 4",
        r#"IF @a = 1
BEGIN
    SELECT
        1
END
ELSE IF @a = 2
BEGIN
    SELECT
        2
END
ELSE
BEGIN
    SELECT
        3
END
SELECT
    4"#,
    );
}

#[test]
fn tsql_if_without_begin_indents_one_statement() {
    assert_layout(
        SqlDialect::Tsql,
        "if not exists (select 1 from sys.tables where name = 't') create table t (id int) else print 'exists'",
        r#"IF NOT EXISTS (SELECT 1 FROM sys.tables WHERE name = 't')
    CREATE TABLE t (
        id INT
    )
ELSE
    PRINT 'exists'"#,
    );
}

#[test]
fn tsql_try_catch_and_cursor_loop() {
    assert_layout(
        SqlDialect::Tsql,
        "begin try declare c cursor local fast_forward for select id from t open c fetch next from c into @id while @@fetch_status = 0 begin print @id fetch next from c into @id end close c deallocate c end try begin catch if xact_state() <> 0 rollback;throw end catch",
        r#"BEGIN TRY
    DECLARE c CURSOR LOCAL FAST_FORWARD FOR
        SELECT
            id
        FROM
            t
    OPEN c
    FETCH NEXT FROM c INTO @id
    WHILE @@fetch_status = 0
    BEGIN
        PRINT @id
        FETCH NEXT FROM c INTO @id
    END
    CLOSE c
    DEALLOCATE c
END TRY
BEGIN CATCH
    IF XACT_STATE() <> 0
        ROLLBACK;
    THROW
END CATCH"#,
    );
}

#[test]
fn tsql_statements_without_semicolons_start_their_own_lines() {
    assert_layout(
        SqlDialect::Tsql,
        "set nocount on set xact_abort on begin tran update t set x = 1 insert into t default values grant execute on dbo.p to app commit tran",
        r#"SET NOCOUNT ON
SET XACT_ABORT ON
BEGIN TRAN
UPDATE t
SET x = 1
INSERT INTO t DEFAULT VALUES
GRANT EXECUTE ON dbo.p TO app
COMMIT TRAN"#,
    );
}

#[test]
fn tsql_alter_table_actions_stay_in_one_statement() {
    assert_layout(
        SqlDialect::Tsql,
        "alter table t alter column a int not null alter table t drop constraint ck alter table t with check add constraint fk foreign key (a) references p (a)",
        r#"ALTER TABLE t ALTER COLUMN a INT NOT NULL
ALTER TABLE t DROP CONSTRAINT ck
ALTER TABLE t WITH CHECK ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES p (a)"#,
    );
}

#[test]
fn comments_inside_a_body_follow_its_indent() {
    assert_layout(
        SqlDialect::Tsql,
        "create or alter proc dbo.p\n  -- param comment\n  @a int = null, -- trailing a\n  @b int = 2\nas\n/* body comment */\nbegin\n  -- inside\n  select @a -- trailing\n  -- before end\nend",
        r#"CREATE OR ALTER PROC dbo.p
    -- param comment
    @a INT = NULL, -- trailing a
    @b INT = 2
AS
/* body comment */
BEGIN
    -- inside
    SELECT
        @a -- trailing
    -- before end
END"#,
    );
}

#[test]
fn mysql_delimiter_script_keeps_its_delimiters() {
    assert_layout(
        SqlDialect::Mysql,
        "delimiter //\ncreate procedure p(in n int)\nbegin\n  declare i int default 0;\n  while i < n do\n    set i = i + 1;\n    if i = 3 then select 'three'; elseif i = 4 then select 'four'; else select i; end if;\n  end while;\n  lbl: loop leave lbl; end loop;\nend//\ndelimiter ;\ncall p(5);",
        r#"delimiter //
CREATE PROCEDURE p(IN n INT)
BEGIN
    DECLARE i INT DEFAULT 0;
    WHILE i < n DO
        SET i = i + 1;
        IF i = 3 THEN
            SELECT
                'three';
        ELSEIF i = 4 THEN
            SELECT
                'four';
        ELSE
            SELECT
                i;
        END IF;
    END WHILE;
    lbl: LOOP
        LEAVE lbl;
    END LOOP;
END //
delimiter ;
CALL p(5);"#,
    );
}

#[test]
fn delimiter_line_marks_a_script_as_mysql() {
    assert_layout(
        SqlDialect::AutoDetect,
        "delimiter //\ncreate procedure p() begin select 1; end//\ndelimiter ;",
        r#"delimiter //
CREATE PROCEDURE p()
BEGIN
    SELECT
        1;
END //
delimiter ;"#,
    );
}

#[test]
fn mysql_trigger_body_is_indented() {
    assert_layout(
        SqlDialect::Mysql,
        "create trigger trg before insert on t for each row begin if new.a is null then set new.a = 0; end if; end",
        r#"CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW
BEGIN
    IF new.a IS NULL THEN
        SET new.a = 0;
    END IF;
END"#,
    );
}

#[test]
fn mysql_quoted_account_names_format() {
    assert_layout(
        SqlDialect::Mysql,
        "create definer=`root`@`localhost` trigger trg before insert on t for each row set new.created = now(); grant select on db.* to 'app'@'%'",
        r#"CREATE definer = `root`@`localhost` TRIGGER trg BEFORE INSERT ON t FOR EACH ROW SET new.created = NOW();
GRANT SELECT ON db.* TO 'app'@'%'"#,
    );
}

#[test]
fn sqlite_trigger_body_is_indented() {
    assert_layout(
        SqlDialect::Sqlite,
        "create trigger trg after insert on t begin insert into log values (new.id); update t set a = 1 where id = new.id; end; select 1;",
        r#"CREATE TRIGGER trg AFTER INSERT ON t
BEGIN
    INSERT INTO log
    VALUES
        (new.id);
    UPDATE t
    SET a = 1
    WHERE
        id = new.id;
END;
SELECT
    1;"#,
    );
}

#[test]
fn a_transaction_begin_is_not_a_block() {
    assert_layout(
        SqlDialect::Postgresql,
        "begin; create table t (id int); commit;",
        r#"BEGIN;
CREATE TABLE t (
    id INT
);
COMMIT;"#,
    );
}

#[test]
fn oracle_block_sections_and_handlers_are_indented() {
    assert_layout(
        SqlDialect::Oracle,
        "declare v number := 0; begin for r in c loop v := v + r.id; if v > 10 then exit; elsif v > 5 then continue; else null; end if; end loop; begin insert into t values (1); exception when dup_val_on_index then null; when others then raise; end; end;\n/",
        r#"DECLARE
    v NUMBER := 0;
BEGIN
    FOR r IN c LOOP
        v := v + r.id;
        IF v > 10 THEN
            exit;
        ELSIF v > 5 THEN
            continue;
        ELSE
            NULL;
        END IF;
    END LOOP;
    BEGIN
        INSERT INTO t
        VALUES
            (1);
    EXCEPTION
        WHEN dup_val_on_index THEN
            NULL;
        WHEN others THEN
            raise;
    END;
END;
/"#,
    );
}

#[test]
fn oracle_package_body_nests_its_routines() {
    assert_layout(
        SqlDialect::Oracle,
        "create or replace package body pkg as g number; procedure p is n number; begin n := 1; end p; function f return number is begin return 1; end f; begin g := 0; end pkg;\n/",
        r#"CREATE OR REPLACE PACKAGE BODY pkg AS
    g NUMBER;
    PROCEDURE p IS
        n NUMBER;
    BEGIN
        n := 1;
    END p;
    FUNCTION f RETURN NUMBER IS
    BEGIN
        RETURN 1;
    END f;
BEGIN
    g := 0;
END pkg;
/"#,
    );
}

#[test]
fn oracle_trigger_declare_section_gets_its_own_line() {
    assert_layout(
        SqlDialect::Oracle,
        "create or replace trigger trg before insert or update on t for each row declare n number; begin if inserting then :new.id := 1; end if; end;\n/",
        r#"CREATE OR REPLACE TRIGGER trg BEFORE INSERT OR UPDATE ON t FOR EACH ROW
DECLARE
    n NUMBER;
BEGIN
    IF inserting THEN
        :new.id := 1;
    END IF;
END;
/"#,
    );
}

#[test]
fn ddl_parens_are_not_function_calls() {
    assert_layout(
        SqlDialect::Tsql,
        "create table dbo.o (id int not null, cid int references dbo.c (id), total as (qty * price) persisted, constraint pk primary key nonclustered (id) include (total)); create view dbo.v (a, b) as select 1, 2",
        r#"CREATE TABLE dbo.o (
    id INT NOT NULL,
    cid INT REFERENCES dbo.c (id),
    total AS (qty * price) PERSISTED,
    CONSTRAINT pk PRIMARY KEY NONCLUSTERED (id) INCLUDE (total)
);
CREATE VIEW dbo.v (a, b) AS
SELECT
    1,
    2"#,
    );
}

#[test]
fn temp_table_keeps_its_hash_under_auto_detect() {
    assert_layout(
        SqlDialect::AutoDetect,
        "create table #t (a int); select * into ##g from #t",
        r#"CREATE TABLE #t (
    a INT
);
SELECT
    * INTO ##g
FROM
    #t"#,
    );
}

#[test]
fn a_list_ending_on_a_comma_keeps_the_comma() {
    assert_layout(
        SqlDialect::Tsql,
        "create table t (a int,",
        r#"CREATE TABLE t (a INT,"#,
    );
}

/// A half-typed condition must not gain parentheses on a second pass.
#[test]
fn dangling_boolean_operator_is_idempotent() {
    let p = SqlFormattingPreferences {
        dialect: SqlDialect::Tsql,
        ..SqlFormattingPreferences::for_preset(Preset::Expanded)
    };
    let sql = "select 1; select a from t where a = 1 and b = 2 or";
    let once = format_sql_report(sql, &p, None).output;
    assert_eq!(format_sql_report(&once, &p, None).output, once);
}

/// Reserved words read as keywords wherever they stand, not only in the
/// clauses the layout knows.
#[test]
fn procedural_words_take_keyword_casing() {
    let out = layout(
        SqlDialect::Tsql,
        "alter table t add column c int collate Latin1_General_CI_AS; while @i < 3 begin print @i break end",
    );
    for word in ["ADD", "COLLATE", "WHILE", "PRINT", "BREAK"] {
        assert!(out.contains(word), "{word} missing in {out}");
    }
}

/// With the dialect left on Auto Detect, a script that only T-SQL could
/// have written is read as T-SQL: `add`, `after` and the `SET` options were
/// coming back in the typed case while everything around them was cased.
#[test]
fn auto_detect_reads_tsql_ddl_as_tsql() {
    assert_layout(
        SqlDialect::AutoDetect,
        "alter table dbo.Orders add Notes nvarchar(max) null, Flag bit not null;",
        "ALTER TABLE dbo.Orders ADD Notes NVARCHAR(MAX) NULL, Flag BIT NOT NULL;",
    );
    assert_layout(
        SqlDialect::AutoDetect,
        "create trigger dbo.trg on dbo.Orders after insert, update as begin set nocount on; select 1; end",
        r#"CREATE TRIGGER dbo.trg
ON dbo.Orders
AFTER INSERT, UPDATE
AS
BEGIN
    SET NOCOUNT ON;
    SELECT
        1;
END"#,
    );
    // No name gives the engine away here: only the `AS BEGIN` block does.
    // Read as PostgreSQL the `END` was glued onto the last select item.
    assert_layout(
        SqlDialect::AutoDetect,
        "create proc p as begin select 1 end",
        r#"CREATE PROC p
AS
BEGIN
    SELECT
        1
END"#,
    );
    assert_layout(
        SqlDialect::AutoDetect,
        "if 1 = 1 begin select a from t end else begin select b from t end",
        layout(
            SqlDialect::Tsql,
            "if 1 = 1 begin select a from t end else begin select b from t end",
        )
        .as_str(),
    );
}

/// DDL words the keyword tables leave out take keyword casing in every
/// dialect, and only where the grammar puts them.
#[test]
fn ddl_words_take_keyword_casing_in_every_dialect() {
    for dialect in [
        SqlDialect::AutoDetect,
        SqlDialect::Postgresql,
        SqlDialect::Tsql,
        SqlDialect::Mysql,
    ] {
        for (sql, expected) in [
            (
                "alter table t add constraint pk primary key (a)",
                "ALTER TABLE t ADD CONSTRAINT pk PRIMARY KEY (a)",
            ),
            (
                "alter table t add constraint fk foreign key (a) references u (a) on delete cascade",
                "ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (a) REFERENCES u (a) ON DELETE CASCADE",
            ),
            ("truncate table t", "TRUNCATE TABLE t"),
            ("drop table t cascade", "DROP TABLE t CASCADE"),
        ] {
            assert_layout(dialect, sql, expected);
        }
    }
    assert_layout(
        SqlDialect::Postgresql,
        "create trigger trg before insert on t for each row execute function f()",
        "CREATE TRIGGER trg BEFORE INSERT ON t FOR EACH ROW EXECUTE FUNCTION f()",
    );
    assert_layout(
        SqlDialect::Postgresql,
        "create sequence s start with 1 increment by 1 no cycle",
        "CREATE SEQUENCE s START WITH 1 INCREMENT BY 1 NO CYCLE",
    );
    assert_layout(
        SqlDialect::Postgresql,
        "alter table t alter column y type int; alter table t add z int",
        "ALTER TABLE t ALTER COLUMN y TYPE INT;\nALTER TABLE t ADD z INT",
    );
    // Columns that share a DDL word keep the spelling they were typed with.
    let out = layout(
        SqlDialect::Postgresql,
        "select add, cascade, start, increment, after, row, action from t where truncate = 1",
    );
    for name in [
        "add",
        "cascade",
        "start",
        "increment",
        "after",
        "row",
        "action",
        "truncate",
    ] {
        assert!(out.contains(name), "{name} was re-cased in {out}");
    }
}
