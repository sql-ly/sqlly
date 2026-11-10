//! Bulk object operations: the pure half of the schema tree's multi-selection
//! actions — grouping the selection into per-source batches and assembling
//! each action's reviewed script. UI glue lives in `app/explorer_power.rs`; script
//! rendering itself lives in `sqlly_services::script_set`.

use sqlly_core::ObjectKind;
use sqlly_lang::sqlmodel::DatabaseType;
use sqlly_services::ScriptSetObject;
use sqlly_services::maintenance::{MaintenanceOp, MaintenanceTarget, maintenance_sql};

use crate::schema_tree::{BulkObjectKind, BulkObjectRef};

/// The slice of a multi-selection living on one server + database. Every bulk
/// action runs per group: a script or export is always against a single
/// source database.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BulkObjectGroup {
    pub server_id: Option<uuid::Uuid>,
    pub database: Option<String>,
    pub objects: Vec<BulkObjectRef>,
}

/// Group the selection by (server, database) in first-appearance order;
/// objects keep click order within their group.
pub fn group_objects(objects: &[BulkObjectRef]) -> Vec<BulkObjectGroup> {
    let mut groups: Vec<BulkObjectGroup> = Vec::new();
    for obj in objects {
        match groups
            .iter_mut()
            .find(|g| g.server_id == obj.server_id && g.database == obj.database)
        {
            Some(group) => group.objects.push(obj.clone()),
            None => groups.push(BulkObjectGroup {
                server_id: obj.server_id,
                database: obj.database.clone(),
                objects: vec![obj.clone()],
            }),
        }
    }
    groups
}

/// The group's objects for the script pipeline (CREATE/DROP rendering).
pub fn script_objects(group: &BulkObjectGroup) -> Vec<ScriptSetObject> {
    group
        .objects
        .iter()
        .map(|o| {
            let kind = match o.kind {
                BulkObjectKind::Table => ObjectKind::Table,
                BulkObjectKind::View => ObjectKind::View,
            };
            ScriptSetObject::new(&o.schema, &o.name, kind)
        })
        .collect()
}

/// The group's tables as (schema, name) pairs for the table-export dialog's
/// pre-check set. Views have no rows to export, so they drop out here.
pub fn export_table_preselect(
    group: &BulkObjectGroup,
) -> std::collections::HashSet<(String, String)> {
    group
        .objects
        .iter()
        .filter(|o| o.kind == BulkObjectKind::Table)
        .map(|o| (o.schema.clone(), o.name.clone()))
        .collect()
}

/// The assembled maintenance script for a group: one engine statement per
/// selected table, in selection order, under a review header. Views are
/// reported in `skipped` (maintenance runs on tables). Fails only when no
/// table produced a statement — an engine with no vocabulary for the op, or a
/// selection of nothing but views.
/// `schema.name`, or the bare name when the engine has no schema.
fn object_label(obj: &BulkObjectRef) -> String {
    if obj.schema.is_empty() {
        obj.name.clone()
    } else {
        format!("{}.{}", obj.schema, obj.name)
    }
}

fn localized_object_kind(kind: BulkObjectKind) -> String {
    let key = match kind {
        BulkObjectKind::Table => "results.bulk_actions.object_type_table",
        BulkObjectKind::View => "results.bulk_actions.object_type_view",
    };
    crate::localization::text(key)
}

pub struct MaintenanceBulkScript {
    pub sql: String,
    pub skipped: Vec<String>,
}

pub fn maintenance_script_for(
    db: DatabaseType,
    op: MaintenanceOp,
    group: &BulkObjectGroup,
) -> Result<MaintenanceBulkScript, String> {
    let mut statements: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();
    for obj in &group.objects {
        if obj.kind != BulkObjectKind::Table {
            skipped.push(crate::localization::format(
                "results.bulk_actions.unsupported_object_kind",
                &[
                    ("object_name", &object_label(obj)),
                    ("object_kind", &localized_object_kind(obj.kind)),
                ],
            ));
            continue;
        }
        let target = MaintenanceTarget::Table {
            schema: (!obj.schema.is_empty()).then(|| obj.schema.clone()),
            name: obj.name.clone(),
        };
        match maintenance_sql(db, op, &target, &[]) {
            // The label lands in a `--` comment: a name carrying a line break
            // must not end the comment and smuggle SQL onto the next line.
            Ok(sql) => statements.push(format!(
                "-- {}\n{sql}",
                sqlly_services::comment_safe(&object_label(obj))
            )),
            Err(error) => skipped.push(crate::localization::format(
                "results.bulk_actions.object_maintenance_error",
                &[
                    ("object_name", &object_label(obj)),
                    ("error", &error.to_string()),
                ],
            )),
        }
    }
    if statements.is_empty() {
        return Err(if skipped.is_empty() {
            crate::localization::text("results.bulk_actions.no_maintenance_selection")
        } else {
            crate::localization::format(
                "results.bulk_actions.maintenance_failed_for_all",
                &[("object_errors", &skipped.join("; "))],
            )
        });
    }
    let review_header = crate::localization::format(
        "results.bulk_actions.maintenance_review_header",
        &[("table_count", &statements.len().to_string())],
    );
    let sql = format!("-- {review_header}\n\n{}", statements.join("\n\n"));
    Ok(MaintenanceBulkScript { sql, skipped })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(
        server: Option<uuid::Uuid>,
        database: Option<&str>,
        schema: &str,
        name: &str,
        kind: BulkObjectKind,
    ) -> BulkObjectRef {
        BulkObjectRef {
            server_id: server,
            database: database.map(str::to_string),
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
        }
    }

    fn table(
        server: Option<uuid::Uuid>,
        database: Option<&str>,
        schema: &str,
        name: &str,
    ) -> BulkObjectRef {
        obj(server, database, schema, name, BulkObjectKind::Table)
    }

    fn view(
        server: Option<uuid::Uuid>,
        database: Option<&str>,
        schema: &str,
        name: &str,
    ) -> BulkObjectRef {
        obj(server, database, schema, name, BulkObjectKind::View)
    }

    #[test]
    fn grouping_follows_first_appearance_and_keeps_click_order() {
        let s1 = uuid::Uuid::new_v4();
        let s2 = uuid::Uuid::new_v4();
        let objects = vec![
            table(Some(s1), Some("shop"), "dbo", "a"),
            table(Some(s2), Some("analytics"), "dbo", "b"),
            view(Some(s1), Some("shop"), "dbo", "v_a"),
            table(Some(s1), Some("other"), "dbo", "c"),
        ];
        let groups = group_objects(&objects);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].database.as_deref(), Some("shop"));
        assert_eq!(groups[0].objects.len(), 2);
        assert_eq!(groups[0].objects[1].name, "v_a");
        assert_eq!(groups[1].database.as_deref(), Some("analytics"));
        assert_eq!(groups[2].database.as_deref(), Some("other"));
    }

    #[test]
    fn script_objects_carry_engine_kinds() {
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![
                table(None, Some("shop"), "dbo", "orders"),
                view(None, Some("shop"), "dbo", "v_orders"),
            ],
        };
        let objects = script_objects(&group);
        assert_eq!(objects[0].kind, ObjectKind::Table);
        assert_eq!(objects[1].kind, ObjectKind::View);
        assert_eq!(objects[0].label(), "dbo.orders");
    }

    #[test]
    fn export_preselect_drops_views() {
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![
                table(None, Some("shop"), "dbo", "orders"),
                view(None, Some("shop"), "dbo", "v_orders"),
            ],
        };
        let set = export_table_preselect(&group);
        assert_eq!(set.len(), 1);
        assert!(set.contains(&("dbo".to_string(), "orders".to_string())));
    }

    #[test]
    fn maintenance_scripts_each_table_and_reports_views() {
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![
                table(None, Some("shop"), "public", "orders"),
                view(None, Some("shop"), "public", "v_orders"),
                table(None, Some("shop"), "public", "customers"),
            ],
        };
        let script =
            maintenance_script_for(DatabaseType::Postgres, MaintenanceOp::VacuumAnalyze, &group)
                .expect("postgres has vacuum analyze");
        assert!(script.sql.contains("orders"));
        assert!(script.sql.contains("customers"));
        assert!(script.sql.to_uppercase().contains("VACUUM"));
        assert_eq!(script.skipped.len(), 1);
        assert!(script.skipped[0].contains("v_orders"));
    }

    #[test]
    fn maintenance_fails_when_nothing_can_run() {
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![view(None, Some("shop"), "public", "v_orders")],
        };
        assert!(
            maintenance_script_for(DatabaseType::Postgres, MaintenanceOp::Vacuum, &group).is_err()
        );
        // An engine with no maintenance vocabulary fails honestly too.
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![table(None, Some("shop"), "dbo", "orders")],
        };
        assert!(
            maintenance_script_for(DatabaseType::Redis, MaintenanceOp::Vacuum, &group).is_err()
        );
    }

    #[test]
    fn maintenance_comment_labels_cannot_inject_statements() {
        let group = BulkObjectGroup {
            server_id: None,
            database: Some("shop".to_string()),
            objects: vec![table(
                None,
                Some("shop"),
                "public",
                "t\nDROP TABLE victims; --",
            )],
        };
        let script = maintenance_script_for(DatabaseType::Postgres, MaintenanceOp::Vacuum, &group)
            .expect("postgres has vacuum");
        // The label stays on its comment line; the statement follows it, and
        // the name's line break survives only inside the quoted identifier.
        let lines: Vec<&str> = script.sql.lines().collect();
        let comment = lines
            .iter()
            .position(|l| l.starts_with("-- public.t"))
            .expect("label comment");
        assert_eq!(lines[comment], "-- public.t DROP TABLE victims; --");
        assert!(
            lines[comment + 1].starts_with("VACUUM \"public\".\"t"),
            "{}",
            script.sql
        );
    }
}
