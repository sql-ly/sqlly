use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::admin::advanced_tools::SchemaObject;
use sqlly_services::admin::schema_plan::{
    DeployOptions, DifferenceStatus, compare_objects, deploy_script, plan_deploy, rollback_script,
};
use sqlly_testkit::golden::GoldenTest;

fn obj(schema: &str, name: &str, kind: &str, def: &str) -> SchemaObject {
    SchemaObject {
        schema: schema.to_string(),
        name: name.to_string(),
        kind: kind.to_string(),
        definition: def.to_string(),
    }
}

fn sqlserver_fixture() -> (Vec<SchemaObject>, Vec<SchemaObject>) {
    let source = vec![
        obj(
            "dbo",
            "Customers",
            "TABLE",
            "Id int NOT NULL, Name nvarchar(50) NULL",
        ),
        obj(
            "dbo",
            "Orders",
            "TABLE",
            "Id int NOT NULL, CustomerId int NOT NULL",
        ),
        obj(
            "dbo",
            "Orders.FK_Orders_Customers",
            "FOREIGN KEY",
            "-- (CustomerId) references dbo.Customers (Id)",
        ),
        obj(
            "dbo",
            "Orders.IX_Orders_CustomerId",
            "INDEX",
            "-- nonclustered (CustomerId)",
        ),
        obj(
            "dbo",
            "vOrders",
            "VIEW",
            "CREATE VIEW dbo.vOrders AS SELECT o.Id FROM dbo.Orders o",
        ),
        obj(
            "dbo",
            "sp_ListOrders",
            "ROUTINE",
            "CREATE PROCEDURE dbo.sp_ListOrders AS SELECT 1",
        ),
        obj(
            "dbo",
            "Orders.trg_Orders",
            "TRIGGER",
            "CREATE TRIGGER dbo.trg_Orders ON dbo.Orders AFTER INSERT AS SELECT 1",
        ),
        obj(
            "dbo",
            "Products",
            "TABLE",
            "Id int NOT NULL, Title nvarchar(100) NULL",
        ),
    ];
    let target = vec![
        obj(
            "dbo",
            "Customers",
            "TABLE",
            "Id int NOT NULL, Name nvarchar(50) NULL",
        ),
        obj(
            "dbo",
            "Orders",
            "TABLE",
            "Id int NOT NULL, CustomerId int NOT NULL",
        ),
        obj(
            "dbo",
            "Orders.FK_Orders_Customers",
            "FOREIGN KEY",
            "-- (CustomerId) references dbo.Customers (Id)",
        ),
        obj(
            "dbo",
            "Orders.IX_Orders_CustomerId",
            "INDEX",
            "-- nonclustered (CustomerId)",
        ),
        obj(
            "dbo",
            "vOrders",
            "VIEW",
            "CREATE VIEW dbo.vOrders AS SELECT o.Id, o.CustomerId FROM dbo.Orders o",
        ),
        obj(
            "dbo",
            "sp_ListOrders",
            "ROUTINE",
            "CREATE PROCEDURE dbo.sp_ListOrders AS SELECT 1",
        ),
        obj(
            "dbo",
            "Orders.trg_Orders",
            "TRIGGER",
            "CREATE TRIGGER dbo.trg_Orders ON dbo.Orders AFTER INSERT AS SELECT 1",
        ),
        obj("dbo", "Legacy", "TABLE", "Id int NOT NULL"),
        obj(
            "dbo",
            "Product",
            "TABLE",
            "Id int NOT NULL, Title nvarchar(100) NULL",
        ),
    ];
    (source, target)
}

fn postgres_fixture() -> (Vec<SchemaObject>, Vec<SchemaObject>) {
    let source = vec![
        obj(
            "public",
            "customers",
            "TABLE",
            "id int NOT NULL, name text NULL",
        ),
        obj(
            "public",
            "orders",
            "TABLE",
            "id int NOT NULL, customer_id int NOT NULL",
        ),
        obj(
            "public",
            "orders.fk_orders_customers",
            "FOREIGN KEY",
            "-- (customer_id) references public.customers (id)",
        ),
        obj(
            "public",
            "orders.ix_orders_customer_id",
            "INDEX",
            "-- btree (customer_id)",
        ),
        obj(
            "public",
            "vorders",
            "VIEW",
            "CREATE VIEW public.vorders AS SELECT o.id FROM public.orders o",
        ),
        obj(
            "public",
            "sp_listorders()",
            "ROUTINE",
            "CREATE FUNCTION public.sp_listorders() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
        ),
        obj(
            "public",
            "orders.trg_orders",
            "TRIGGER",
            "CREATE TRIGGER trg_orders AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION sp_listorders()",
        ),
        obj(
            "public",
            "products",
            "TABLE",
            "id int NOT NULL, title text NULL",
        ),
    ];
    let target = vec![
        obj(
            "public",
            "customers",
            "TABLE",
            "id int NOT NULL, name text NULL",
        ),
        obj(
            "public",
            "orders",
            "TABLE",
            "id int NOT NULL, customer_id int NOT NULL",
        ),
        obj(
            "public",
            "orders.fk_orders_customers",
            "FOREIGN KEY",
            "-- (customer_id) references public.customers (id)",
        ),
        obj(
            "public",
            "orders.ix_orders_customer_id",
            "INDEX",
            "-- btree (customer_id)",
        ),
        obj(
            "public",
            "vorders",
            "VIEW",
            "CREATE VIEW public.vorders AS SELECT o.id, o.customer_id FROM public.orders o",
        ),
        obj(
            "public",
            "sp_listorders()",
            "ROUTINE",
            "CREATE FUNCTION public.sp_listorders() RETURNS int AS $$ SELECT 1 $$ LANGUAGE sql",
        ),
        obj(
            "public",
            "orders.trg_orders",
            "TRIGGER",
            "CREATE TRIGGER trg_orders AFTER INSERT ON public.orders FOR EACH ROW EXECUTE FUNCTION sp_listorders()",
        ),
        obj("public", "legacy", "TABLE", "id int NOT NULL"),
        obj(
            "public",
            "product",
            "TABLE",
            "id int NOT NULL, title text NULL",
        ),
    ];
    (source, target)
}

fn plan_for(db: DatabaseType) -> sqlly_services::admin::schema_plan::DeployPlan {
    let (source, target) = match db {
        DatabaseType::SqlServer => sqlserver_fixture(),
        DatabaseType::Postgres => postgres_fixture(),
        _ => panic!("unsupported golden db"),
    };
    let mut differences = compare_objects(&source, &target, db, db);
    let renames = sqlly_services::admin::schema_plan::detect_renames(&differences);
    sqlly_services::admin::schema_plan::apply_renames(&mut differences, &renames);
    plan_deploy(
        &differences,
        db,
        db,
        DeployOptions {
            include_drops: true,
            ..Default::default()
        },
    )
}

fn golden_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/admin_golden")
        .join(name)
}

#[test]
fn golden_sqlserver_deploy_and_rollback() {
    let plan = plan_for(DatabaseType::SqlServer);
    let deploy = deploy_script(&plan, "-- golden deploy");
    let rollback = rollback_script(&plan, "-- golden rollback");
    assert!(
        GoldenTest::compare(&deploy, golden_path("sqlserver_deploy.sql")).unwrap(),
        "deploy script diverged from golden; rerun with SQLLY_UPDATE_GOLDEN=1 to refresh:\n{deploy}"
    );
    assert!(
        GoldenTest::compare(&rollback, golden_path("sqlserver_rollback.sql")).unwrap(),
        "rollback script diverged from golden; rerun with SQLLY_UPDATE_GOLDEN=1 to refresh:\n{rollback}"
    );
    assert_golden_ordering(&plan);
}

#[test]
fn golden_postgres_deploy_and_rollback() {
    let plan = plan_for(DatabaseType::Postgres);
    let deploy = deploy_script(&plan, "-- golden deploy");
    let rollback = rollback_script(&plan, "-- golden rollback");
    assert!(
        GoldenTest::compare(&deploy, golden_path("postgres_deploy.sql")).unwrap(),
        "deploy script diverged from golden; rerun with SQLLY_UPDATE_GOLDEN=1 to refresh:\n{deploy}"
    );
    assert!(
        GoldenTest::compare(&rollback, golden_path("postgres_rollback.sql")).unwrap(),
        "rollback script diverged from golden; rerun with SQLLY_UPDATE_GOLDEN=1 to refresh:\n{rollback}"
    );
    assert_golden_ordering(&plan);
}

fn assert_golden_ordering(plan: &sqlly_services::admin::schema_plan::DeployPlan) {
    let table_idx = plan
        .steps
        .iter()
        .position(|s| s.kind == "TABLE" && s.identity.contains("Orders"));
    let fk_idx = plan.steps.iter().position(|s| s.kind == "FOREIGN KEY");
    let view_idx = plan.steps.iter().position(|s| s.kind == "VIEW");
    if let (Some(table_idx), Some(fk_idx)) = (table_idx, fk_idx) {
        assert!(table_idx < fk_idx);
    }
    if let (Some(table_idx), Some(view_idx)) = (table_idx, view_idx) {
        assert!(table_idx < view_idx);
    }
    let first_drop = plan
        .steps
        .iter()
        .position(|s| matches!(s.status, DifferenceStatus::ExtraInTarget));
    let last_create = plan.steps.iter().rposition(|s| {
        matches!(
            s.status,
            DifferenceStatus::MissingInTarget
                | DifferenceStatus::Changed
                | DifferenceStatus::Renamed { .. }
        )
    });
    if let (Some(create), Some(drop)) = (last_create, first_drop) {
        assert!(create < drop);
    }
    let runnable: Vec<_> = plan.steps.iter().filter(|s| s.runnable).collect();
    let rollback = rollback_script(plan, "");
    for step in runnable.iter().rev() {
        assert!(rollback.contains(step.rollback_sql.lines().next().unwrap_or("")));
    }
}
