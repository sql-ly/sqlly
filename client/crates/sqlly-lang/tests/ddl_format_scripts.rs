//! CREATE / ALTER DDL and procedural scripts through the formatter: nothing
//! declines, panics the token safety check, or changes on a second pass, at
//! any point while the script is being typed.

use sqlly_lang::sqlformat::*;

const T: SqlDialect = SqlDialect::Tsql;
const P: SqlDialect = SqlDialect::Postgresql;
const M: SqlDialect = SqlDialect::Mysql;
const S: SqlDialect = SqlDialect::Sqlite;
const O: SqlDialect = SqlDialect::Oracle;

const CASES: &[(SqlDialect, &str)] = &[
    (
        T,
        "create procedure dbo.GetOrders @CustomerId int, @From datetime2 = null, @Top int = 10 output as begin set nocount on; select top (@Top) o.OrderId, o.Total from dbo.Orders o where o.CustomerId = @CustomerId and (@From is null or o.CreatedAt >= @From) order by o.CreatedAt desc; end",
    ),
    (
        T,
        "CREATE PROCEDURE dbo.GetOrders\n    @CustomerId INT,\n    @From DATETIME2 = NULL\nAS\nBEGIN\n    SET NOCOUNT ON;\n\n    SELECT o.OrderId, o.Total\n    FROM dbo.Orders o\n    WHERE o.CustomerId = @CustomerId;\nEND\nGO",
    ),
    (
        T,
        "create or alter procedure dbo.p (@a int, @b nvarchar(max)) with recompile, execute as owner as begin try begin transaction; update dbo.t set x = @a where y = @b; commit; end try begin catch if @@trancount > 0 rollback; throw; end catch",
    ),
    (T, "alter proc p as select 1"),
    (
        T,
        "create proc p as if exists (select 1 from t) begin print 'x' end else begin print 'y' end",
    ),
    (
        T,
        "create proc p @x int as declare @i int = 0; while @i < @x begin set @i = @i + 1; if @i = 5 continue; if @i > 8 break; end; return @i;",
    ),
    (
        T,
        "create function dbo.CalcTotal(@OrderId int) returns decimal(18,2) as begin declare @t decimal(18,2); select @t = sum(Quantity * UnitPrice) from dbo.OrderLines where OrderId = @OrderId; return @t; end",
    ),
    (
        T,
        "create function dbo.OrdersFor(@CustomerId int) returns table as return (select OrderId, Total from dbo.Orders where CustomerId = @CustomerId)",
    ),
    (
        T,
        "create function dbo.Split(@s nvarchar(max)) returns @out table (Item nvarchar(100) not null, Pos int) with schemabinding as begin insert into @out select value, 1 from string_split(@s, ','); return; end",
    ),
    (
        T,
        "create view dbo.ActiveOrders with schemabinding as select o.OrderId, o.Total, c.Name from dbo.Orders o join dbo.Customers c on c.CustomerId = o.CustomerId where o.Status = 'A' with check option",
    ),
    (
        T,
        "create view v (a, b) as select 1, 2 union all select 3, 4",
    ),
    (
        T,
        "create trigger dbo.trg_Orders_Audit on dbo.Orders after insert, update as begin set nocount on; insert into dbo.Audit (OrderId, ChangedAt) select i.OrderId, sysutcdatetime() from inserted i; end",
    ),
    (
        T,
        "create trigger trg on dbo.t instead of delete as begin update t set Deleted = 1 from dbo.t t join deleted d on d.Id = t.Id end",
    ),
    (
        T,
        "create table dbo.Orders (OrderId int identity(1,1) not null constraint PK_Orders primary key clustered, CustomerId int not null constraint FK_Orders_Customers foreign key references dbo.Customers (CustomerId) on delete cascade, Total decimal(18, 2) not null constraint DF_Orders_Total default (0), Status char(1) not null check (Status in ('A', 'B')), CreatedAt datetime2(3) not null default sysutcdatetime(), LineTotal as (Quantity * UnitPrice) persisted, RowVer rowversion, index IX_Orders_Cust nonclustered (CustomerId) include (Total)) on [PRIMARY] with (data_compression = page)",
    ),
    (
        T,
        "create table #tmp (a int, b varchar(10) collate Latin1_General_CI_AS null)",
    ),
    (
        T,
        "declare @t table (Id int primary key, Name nvarchar(50))",
    ),
    (
        T,
        "create type dbo.IdList as table (Id int not null primary key)",
    ),
    (T, "create type dbo.Money2 from decimal(18,2) not null"),
    (
        T,
        "alter table dbo.Orders add Notes nvarchar(max) null, Flag bit not null constraint DF_Flag default 0",
    ),
    (
        T,
        "alter table dbo.Orders add constraint FK_x foreign key (CustomerId) references dbo.Customers (CustomerId)",
    ),
    (
        T,
        "alter table dbo.Orders alter column Total decimal(19,4) not null",
    ),
    (
        T,
        "alter table dbo.Orders drop column Notes, constraint DF_Flag",
    ),
    (
        T,
        "create unique nonclustered index IX_Orders on dbo.Orders (CustomerId asc, CreatedAt desc) include (Total) where Status = 'A' with (fillfactor = 90, online = on) on [PRIMARY]",
    ),
    (T, "create clustered columnstore index cci on dbo.Fact"),
    (T, "create schema Sales authorization dbo"),
    (
        T,
        "create sequence dbo.Seq as bigint start with 1 increment by 1 minvalue 1 no cycle cache 50",
    ),
    (T, "create synonym dbo.O for OtherDb.dbo.Orders"),
    (T, "create database Shop collate Latin1_General_100_CI_AS"),
    (
        T,
        "create login app with password = 'x', check_policy = off; create user app for login app with default_schema = dbo; grant select, insert on schema::dbo to app;",
    ),
    (
        T,
        "if object_id('dbo.p', 'P') is not null drop procedure dbo.p;\ngo\ncreate procedure dbo.p as select 1;\ngo\nexec dbo.p;",
    ),
    (
        T,
        "create proc p as\n-- header comment\nselect 1 -- trailing\n/* block */\nselect 2",
    ),
    (
        T,
        "create proc p as begin merge dbo.t as tgt using (select 1 as id) as src on tgt.id = src.id when matched then update set x = 1 when not matched then insert (id) values (src.id) output $action, inserted.id; end",
    ),
    (
        T,
        "create proc p as begin declare c cursor local fast_forward for select id from t; open c; fetch next from c into @id; while @@fetch_status = 0 begin exec dbo.q @id; fetch next from c into @id; end; close c; deallocate c; end",
    ),
    (
        T,
        "create proc p as begin select case when a = 1 then 'x' else 'y' end as k from t; end",
    ),
    (
        T,
        "create proc p as begin; with cte as (select 1 as a) select * from cte; end",
    ),
    (
        T,
        "create proc p @a int as begin if @a = 1 select 1 else if @a = 2 select 2 else select 3 end",
    ),
    (
        T,
        "create proc p as exec sp_executesql N'select 1 where 1 = @a', N'@a int', @a = 1",
    ),
    (T, "create table t (\n"),
    (T, "create table t (a int,"),
    (T, "create proc p as begin"),
    (
        P,
        "create or replace function add_one(i integer) returns integer language plpgsql as $$ begin return i + 1; end; $$",
    ),
    (
        P,
        "create function f(a int, b text default 'x') returns table (id int, name text) as $body$ select id, name from t where id = a $body$ language sql stable",
    ),
    (
        P,
        "create or replace procedure p(a int) language plpgsql as $$ declare n int; begin select count(*) into n from t; if n > 0 then raise notice 'x'; end if; end $$",
    ),
    (
        P,
        "create table if not exists public.users (id bigserial primary key, email text not null unique, tags text[] default '{}', meta jsonb, created_at timestamptz not null default now(), constraint ck check (char_length(email) > 3)) partition by range (created_at)",
    ),
    (
        P,
        "create materialized view mv as select a, count(*) from t group by a with no data",
    ),
    (P, "create or replace view v as select * from t"),
    (
        P,
        "create trigger trg before insert or update on users for each row execute function set_ts()",
    ),
    (
        P,
        "create index concurrently if not exists ix on users using gin (meta jsonb_path_ops) where meta is not null",
    ),
    (P, "create type mood as enum ('sad', 'ok', 'happy')"),
    (P, "create extension if not exists pgcrypto"),
    (
        P,
        "alter table users add column age int check (age > 0), alter column email set not null, drop column if exists old",
    ),
    (
        P,
        "create policy p on users for select using (tenant_id = current_setting('app.tenant')::int)",
    ),
    (P, "do $$ begin perform 1; end $$"),
    (
        M,
        "create table `users` (`id` int unsigned not null auto_increment, `name` varchar(100) character set utf8mb4 not null default '', `created` timestamp default current_timestamp on update current_timestamp, primary key (`id`), key `ix_name` (`name`), constraint `fk` foreign key (`id`) references `o` (`id`)) engine=InnoDB default charset=utf8mb4",
    ),
    (
        M,
        "create procedure p(in a int, out b int) begin select count(*) into b from t where x = a; end",
    ),
    (
        M,
        "create definer=`root`@`localhost` trigger trg before insert on t for each row set new.created = now()",
    ),
    (
        M,
        "create function f(a int) returns int deterministic return a + 1",
    ),
    (
        M,
        "delimiter //\ncreate procedure p() begin select 1; end//\ndelimiter ;",
    ),
    (
        S,
        "create table if not exists notes (id integer primary key autoincrement, body text not null default '', created_at text default (datetime('now'))) without rowid",
    ),
    (
        S,
        "create trigger trg after update of body on notes when new.body <> old.body begin update notes set created_at = datetime('now') where id = new.id; end",
    ),
    (
        S,
        "create virtual table ft using fts5(body, content='notes')",
    ),
    (S, "create view if not exists v as select id from notes"),
    (
        O,
        "create or replace procedure p (a in number, b out varchar2) is n number; begin select count(*) into n from t; b := to_char(n); exception when others then null; end p;",
    ),
    (
        O,
        "create or replace function f (a number) return number is begin return a + 1; end;",
    ),
    (
        O,
        "create or replace package pkg as procedure p; function f return number; end pkg;",
    ),
    (
        O,
        "create or replace trigger trg before insert on t for each row begin :new.id := seq.nextval; end;",
    ),
    (
        O,
        "create table t (id number(10) generated always as identity, name varchar2(100 char) not null, constraint pk primary key (id)) tablespace users",
    ),
];

/// Scripts that stress statement boundaries inside procedural bodies.
const BODIES: &[(SqlDialect, &str)] = &[
    (T, r#"select 1 select 2"#),
    (
        T,
        r#"insert into t (a) select a from s insert into t2 exec dbo.p update t set a = 1 delete from t where a = 2"#,
    ),
    (
        T,
        r#"with a as (select 1 x) insert into t select x from a; with b as (select 2 y) select * from b"#,
    ),
    (
        T,
        r#"select a, case when b = 1 then 'begin' else 'end' end as c from t where d in (select d from u) union all select 1, 'x'"#,
    ),
    (
        T,
        r#"update t set a = 1, b = 2 output inserted.a into @log where c = 3"#,
    ),
    (
        T,
        r#"create table t (id int, [set] int, [select] int); insert t values (1, 2, 3)"#,
    ),
    (
        T,
        r#"begin tran
update t set x = 1
if @@error <> 0 begin rollback tran return end
commit tran"#,
    ),
    (
        T,
        r#"if not exists (select 1 from sys.tables where name = 't') create table t (id int) else print 'exists'"#,
    ),
    (
        T,
        r#"drop table if exists dbo.t; drop procedure if exists dbo.p"#,
    ),
    (
        T,
        r#"set @x = (select max(id) from t) set @y = @x + 1 select @x, @y"#,
    ),
    (
        T,
        r#"declare @a int, @b varchar(10) = 'x', @c table (id int)
select @a = 1"#,
    ),
    (
        T,
        r#"exec dbo.p @a = 1, @b = 'x' exec('select 1') execute sp_help 't'"#,
    ),
    (T, r#"select * from t for xml path('row'), root('rows')"#),
    (T, r#"select * from t option (recompile)"#),
    (
        T,
        r#"merge into t using s on t.id = s.id when matched and s.del = 1 then delete when matched then update set t.a = s.a when not matched by target then insert (id, a) values (s.id, s.a) when not matched by source then delete;"#,
    ),
    (
        T,
        r#"create trigger trg on t for insert, update, delete not for replication as if update(col) begin raiserror('no', 16, 1) rollback end"#,
    ),
    (
        T,
        r#"alter procedure dbo.p @ids dbo.IdList readonly, @n int out with execute as caller as set nocount on select * from @ids"#,
    ),
    (
        T,
        r#"create function dbo.f (@a int) returns int with schemabinding, returns null on null input as begin return (@a * 2) end"#,
    ),
    (
        T,
        r#"begin try
  select 1/0
end try
begin catch
  select error_number() as n, error_message() as m
  if xact_state() <> 0 rollback
  ;throw
end catch"#,
    ),
    (
        T,
        r#"while 1 = 1 begin fetch next from cur into @a, @b if @@fetch_status <> 0 break print @a end"#,
    ),
    (T, r#"label1: select 1 goto label1"#),
    (
        T,
        r#"grant execute on dbo.p to app with grant option revoke select on t from app deny update on t to app"#,
    ),
    (
        T,
        r#"select top 10 with ties * from t order by a offset 5 rows fetch next 10 rows only"#,
    ),
    (
        T,
        r#"create view dbo.v as with c as (select 1 a) select a from c"#,
    ),
    (
        T,
        r#"alter table t add constraint df default (getdate()) for created, constraint ck check (a > 0)"#,
    ),
    (
        T,
        r#"alter index ix on t rebuild with (online = on); alter database db set recovery simple; alter table t set (lock_escalation = auto)"#,
    ),
    (
        T,
        r#"insert into t default values insert into t (a) values (1), (2)"#,
    ),
    (
        T,
        r#"use master
go
create database x
go
use x
go
create table t(id int)"#,
    ),
    (T, r#"select 1 as [begin], 2 as [end] from t as [if]"#),
    (
        T,
        r#"if @a = 1 begin select 1 end else if @a = 2 begin select 2 end else begin select 3 end
select 4"#,
    ),
    (
        T,
        r#"if @a = 1
  if @b = 2 select 1
  else select 2
else select 3
select 4"#,
    ),
    (
        T,
        r#"waitfor delay '00:00:01' truncate table t dbcc checkident ('t', reseed, 0) save tran sp1 rollback tran sp1"#,
    ),
    (
        T,
        r#"create proc p as begin set transaction isolation level read uncommitted; set identity_insert dbo.t on; set statistics io on; end"#,
    ),
    (
        T,
        r#"select * from openrowset(bulk 'x', single_clob) as j cross apply openjson(j.BulkColumn) with (id int '$.id', name nvarchar(50) '$.name')"#,
    ),
    (
        T,
        r#"create or alter proc dbo.p
  -- param comment
  @a int = null, -- trailing a
  @b int /* inline */ = 2
as
/* body comment */
begin
  -- inside
  select @a -- trailing
  -- before end
end"#,
    ),
    (
        T,
        r#"update t set a = case when b = 1 then 2 else 3 end, c = 4 where d = 5 delete t where a = 1"#,
    ),
    (
        T,
        r#"select a from t where b = 1 if @@rowcount = 0 insert into t (a) values (1) else update t set a = 2"#,
    ),
    (
        T,
        r#"begin
  declare @sql nvarchar(max) = N'select 1'
  exec sp_executesql @sql
end"#,
    ),
    (
        T,
        r#"create table dbo.t (id int not null, constraint pk primary key (id)) create index ix on dbo.t (id) create unique index ux on dbo.t (id) where id > 0"#,
    ),
    (
        T,
        r#"select * from t1 a inner merge join t2 b on a.id = b.id"#,
    ),
    (
        T,
        r#"insert into dbo.log (msg) output inserted.id into @ids select msg from src"#,
    ),
    (T, r#"delete from t output deleted.* where a = 1"#),
    (
        T,
        r#"set nocount on set xact_abort on begin transaction commit transaction"#,
    ),
    (
        T,
        r#"select 1 union select 2 except select 3 intersect select 4"#,
    ),
    (T, r#"create procedure p as return 0"#),
    (
        T,
        r#"open symmetric key k decryption by certificate c close symmetric key k"#,
    ),
    (
        T,
        r#"select a.b.c.d from x.y.z where [update] = 1 and t.[delete] = 2 and t.set = 3"#,
    ),
    (
        T,
        r#"bulk insert dbo.t from 'f.csv' with (fieldterminator = ',')"#,
    ),
    (
        T,
        r#"create trigger trg on database for create_table, alter_table as print 'ddl'"#,
    ),
    (
        T,
        r#"alter table t with check add constraint fk foreign key (a) references p (a) on update no action alter table t check constraint fk"#,
    ),
    (T, r#"if exists(select 1) select 1"#),
    (T, r#"return"#),
    (T, r#"end"#),
    (T, r#"else select 1"#),
    (T, r#"begin begin begin select 1 end end"#),
    (
        M,
        r#"delimiter //
create procedure p(in n int)
begin
  declare i int default 0;
  declare continue handler for not found set i = -1;
  while i < n do
    set i = i + 1;
    if i = 3 then
      select 'three';
    elseif i = 4 then
      select 'four';
    else
      select i;
    end if;
  end while;
  repeat set i = i - 1; until i = 0 end repeat;
  lbl: loop
    leave lbl;
  end loop;
  case i when 1 then select 1; else select 2; end case;
end//
delimiter ;
call p(5);"#,
    ),
    (
        M,
        r#"create trigger trg before insert on t for each row begin if new.a is null then set new.a = 0; end if; end"#,
    ),
    (
        M,
        r#"create function f(a int) returns int deterministic begin declare r int; set r = a * 2; return r; end"#,
    ),
    (M, r#"start transaction; insert into t values (1); commit;"#),
    (M, r#"begin; update t set a = 1; commit;"#),
    (
        M,
        r#"create event ev on schedule every 1 day do delete from log where created < now() - interval 30 day"#,
    ),
    (
        M,
        r#"select if(a > 1, 'x', 'y'), repeat('a', 3) from t; insert into t (a) values (1) on duplicate key update a = values(a);"#,
    ),
    (
        P,
        r#"begin; create table t (id int); insert into t values (1); commit;"#,
    ),
    (
        P,
        r#"begin transaction isolation level serializable; select 1; end;"#,
    ),
    (
        P,
        r#"create rule r as on insert to t do instead nothing; create or replace function f() returns trigger language plpgsql as $$ begin new.x := 1; return new; end $$; create trigger tg before insert on t for each row execute function f();"#,
    ),
    (
        P,
        r#"create function sq(i int) returns int begin atomic select i * i; end;"#,
    ),
    (
        P,
        r#"alter table t add column a int, drop column b; comment on table t is 'x'; grant select on t to r; create index on t (a) where a > 0;"#,
    ),
    (
        P,
        r#"select case when a then 1 else 2 end, b from t where c; end; begin; declare c cursor for select 1; fetch next from c; close c; commit;"#,
    ),
    (
        O,
        r#"declare
  cursor c is select id from t;
  v number := 0;
begin
  for r in c loop
    v := v + r.id;
    if v > 10 then
      exit;
    elsif v > 5 then
      continue;
    else
      null;
    end if;
  end loop;
  while v > 0 loop v := v - 1; end loop;
  begin
    insert into t values (1);
  exception
    when dup_val_on_index then null;
    when others then raise;
  end;
  dbms_output.put_line(v);
end;
/"#,
    ),
    (
        O,
        r#"create or replace package body pkg as
  g number;
  procedure p is
    n number;
  begin
    n := 1;
  end p;
  function f return number is
  begin
    return 1;
  end f;
begin
  g := 0;
end pkg;
/"#,
    ),
    (
        O,
        r#"create or replace trigger trg before insert or update on t for each row declare n number; begin if inserting then :new.id := 1; end if; end;
/"#,
    ),
    (
        O,
        r#"select case when a = 1 then 'x' else 'y' end from dual; begin null; end;"#,
    ),
    (
        O,
        r#"create or replace view v as select * from t; create table t2 as select * from t; create sequence s start with 1; grant select on t to u;"#,
    ),
    (
        O,
        r#"create or replace function f return number as language java name 'X.f() return int';"#,
    ),
    (
        O,
        r#"create or replace type body tb as member function f return number is begin return 1; end; end;"#,
    ),
    (
        S,
        r#"begin; insert into t values (1); end; create trigger trg after insert on t begin insert into log values (new.id); update t set a = 1 where id = new.id; end; select 1;"#,
    ),
    (
        S,
        r#"create trigger trg instead of insert on v for each row begin select case when new.a is null then raise(abort, 'x') end; end;"#,
    ),
    (
        SqlDialect::AutoDetect,
        r#"create proc p as begin select 1 end"#,
    ),
    (
        SqlDialect::AutoDetect,
        r#"delimiter //
create procedure p() begin select 1; end//
delimiter ;"#,
    ),
];

fn all_cases() -> impl Iterator<Item = &'static (SqlDialect, &'static str)> {
    CASES.iter().chain(BODIES)
}

fn prefs(preset: Preset, dialect: SqlDialect) -> SqlFormattingPreferences {
    let mut p = SqlFormattingPreferences::for_preset(preset);
    p.dialect = dialect;
    p
}

#[test]
fn ddl_scripts_format_without_declining() {
    for (dialect, sql) in all_cases() {
        let report = format_sql_report(sql, &prefs(Preset::Default, *dialect), None);
        assert_eq!(report.declined, None, "{dialect:?} declined: {sql}");
    }
}

#[test]
fn ddl_scripts_are_idempotent_under_every_preset() {
    for (dialect, sql) in all_cases() {
        for preset in Preset::all().iter().copied() {
            for d in [*dialect, SqlDialect::AutoDetect] {
                let p = prefs(preset, d);
                // The debug assertion inside format_sql_report turns any
                // token corruption into a panic.
                let once = format_sql_report(sql, &p, None).output;
                let twice = format_sql_report(&once, &p, None).output;
                assert_eq!(once, twice, "not idempotent under {preset:?}/{d:?}: {sql}");
            }
        }
    }
}

/// Format as you go runs the formatter on half-typed statements, so every
/// prefix of a script has to survive it too.
#[test]
fn every_prefix_of_a_ddl_script_formats_safely() {
    for (dialect, sql) in all_cases() {
        for preset in [Preset::Default, Preset::Compact, Preset::Expanded] {
            for d in [*dialect, SqlDialect::AutoDetect] {
                let p = prefs(preset, d);
                for end in (1..=sql.len()).filter(|e| sql.is_char_boundary(*e)) {
                    let prefix = &sql[..end];
                    let once = format_sql_report(prefix, &p, None).output;
                    let twice = format_sql_report(&once, &p, None).output;
                    assert_eq!(
                        once, twice,
                        "not idempotent under {preset:?}/{d:?}: {prefix:?}"
                    );
                }
            }
        }
    }
}
