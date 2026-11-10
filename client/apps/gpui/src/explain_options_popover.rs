//! Explain-options popover helpers and content.
//!
//! Pure flag/label helpers are unit-tested without a window. The popover
//! itself is assembled from gpui-component `Popover` + `Checkbox`.

use crate::tooltip::AppTooltip;
use gpui::{
    Entity, InteractiveElement as _, IntoElement, ParentElement as _, SharedString, Styled, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::Sizable as _;
use gpui_component::button::{Button, ButtonVariants as _};
use gpui_component::checkbox::Checkbox;
use sqlly_services::plans::{ExplainOptions, ExplainPreset, PlanDialect, ServerVersion};

use crate::app::AppView;
use crate::theme::Theme;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlagRow {
    pub key: &'static str,
    pub label: String,
    pub enabled: bool,
    pub reason: Option<&'static str>,
}

/// Why Profile must not run `sql`, or `None` when SELECT-only.
pub fn profile_block_reason(sql: &str) -> Option<String> {
    let classification = sqlly_intel::query::safety::classify_select_only(sql);
    if classification.select_only {
        None
    } else {
        classification.reason
    }
}

pub fn flag_rows(dialect: PlanDialect, version: Option<ServerVersion>) -> Vec<FlagRow> {
    match dialect {
        PlanDialect::Postgres => ExplainOptions::pg_flag_availability(version)
            .into_iter()
            .map(|(key, enabled, reason)| FlagRow {
                key,
                label: pg_flag_label(key),
                enabled,
                reason: if enabled { None } else { Some(reason) },
            })
            .collect(),
        PlanDialect::MySql | PlanDialect::MariaDb => vec![FlagRow {
            key: "analyze",
            label: crate::localization::text("query.explain_options.flag_analyze"),
            enabled: true,
            reason: None,
        }],
        PlanDialect::SqlServer
        | PlanDialect::Sqlite
        | PlanDialect::DuckDb
        | PlanDialect::ClickHouse
        | PlanDialect::Oracle => Vec::new(),
    }
}

fn pg_flag_label(key: &str) -> String {
    let key = match key {
        "analyze" => "query.explain_options.flag_analyze",
        "verbose" => "query.explain_options.flag_verbose",
        "costs" => "query.explain_options.flag_costs",
        "buffers" => "query.explain_options.flag_buffers",
        "timing" => "query.explain_options.flag_timing",
        "summary" => "query.explain_options.flag_summary",
        "settings" => "query.explain_options.flag_settings",
        "wal" => "query.explain_options.flag_wal",
        "generic_plan" => "query.explain_options.flag_generic_plan",
        "memory" => "query.explain_options.flag_memory",
        _ => "query.explain_options.flag_unknown",
    };
    crate::localization::text(key)
}

pub fn options_summary(opts: &ExplainOptions, dialect: PlanDialect) -> String {
    match dialect {
        PlanDialect::Postgres => {
            let mut flags = Vec::new();
            if opts.pg.analyze {
                flags.push("ANALYZE");
            }
            if opts.pg.verbose {
                flags.push("VERBOSE");
            }
            if opts.pg.buffers {
                flags.push("BUFFERS");
            }
            if opts.pg.settings {
                flags.push("SETTINGS");
            }
            if opts.pg.wal {
                flags.push("WAL");
            }
            if opts.pg.generic_plan {
                flags.push("GENERIC_PLAN");
            }
            if opts.pg.memory {
                flags.push("MEMORY");
            }
            flags.join(", ")
        }
        PlanDialect::MySql | PlanDialect::MariaDb => {
            if opts.mysql.analyze {
                "ANALYZE".to_string()
            } else {
                String::new()
            }
        }
        PlanDialect::SqlServer
        | PlanDialect::Sqlite
        | PlanDialect::DuckDb
        | PlanDialect::ClickHouse
        | PlanDialect::Oracle => String::new(),
    }
}

pub fn version_line(dialect: PlanDialect, cached: Option<&Option<ServerVersion>>) -> SharedString {
    match cached {
        None => crate::localization::text("query.explain_options.version_probing").into(),
        Some(None) => crate::localization::text("query.explain_options.version_unknown").into(),
        Some(Some(v)) => crate::localization::format(
            "query.explain_options.version_known",
            &[
                ("product", dialect_product(dialect)),
                ("major", &v.major.to_string()),
                ("minor", &v.minor.to_string()),
            ],
        )
        .into(),
    }
}

fn dialect_product(dialect: PlanDialect) -> &'static str {
    match dialect {
        PlanDialect::Postgres => "PostgreSQL",
        PlanDialect::MySql => "MySQL",
        PlanDialect::MariaDb => "MariaDB",
        PlanDialect::SqlServer => "SQL Server",
        PlanDialect::Sqlite => "SQLite",
        PlanDialect::DuckDb => "DuckDB",
        PlanDialect::ClickHouse => "ClickHouse",
        PlanDialect::Oracle => "Oracle Database",
    }
}

fn flag_checked(opts: &ExplainOptions, dialect: PlanDialect, key: &str) -> bool {
    match dialect {
        PlanDialect::Postgres => match key {
            "analyze" => opts.pg.analyze,
            "verbose" => opts.pg.verbose,
            "costs" => opts.pg.costs,
            "buffers" => opts.pg.buffers,
            "timing" => opts.pg.timing,
            "summary" => opts.pg.summary,
            "settings" => opts.pg.settings,
            "wal" => opts.pg.wal,
            "generic_plan" => opts.pg.generic_plan,
            "memory" => opts.pg.memory,
            _ => false,
        },
        PlanDialect::MySql | PlanDialect::MariaDb => key == "analyze" && opts.mysql.analyze,
        PlanDialect::SqlServer
        | PlanDialect::Sqlite
        | PlanDialect::DuckDb
        | PlanDialect::ClickHouse
        | PlanDialect::Oracle => false,
    }
}

pub struct ExplainPopoverSnapshot {
    pub connected: bool,
    pub dialect: Option<PlanDialect>,
    pub version: Option<ServerVersion>,
    pub version_text: SharedString,
    pub options: ExplainOptions,
    /// Why an actual-plan capture (the Actual and Profile presets) cannot run
    /// on the active engine, or `None` when it can.
    pub actual_unavailable: Option<String>,
    /// Why an estimated-plan capture cannot run on the active engine.
    pub estimated_unavailable: Option<String>,
}

impl ExplainPopoverSnapshot {
    /// The disabled reason for a preset button, from the plan capability.
    pub fn preset_unavailable(&self, preset: ExplainPreset) -> Option<&str> {
        match preset {
            ExplainPreset::Actual | ExplainPreset::Profile => self.actual_unavailable.as_deref(),
            ExplainPreset::Estimated => self.estimated_unavailable.as_deref(),
        }
    }
}

pub fn render_explain_options(
    snapshot: ExplainPopoverSnapshot,
    host: Entity<AppView>,
) -> gpui::AnyElement {
    let s = Theme::scaled;
    let Some(dialect) = snapshot.dialect.filter(|_| snapshot.connected) else {
        // Disconnected: prompt to connect. Connected to an engine without
        // plans: the snapshot's text carries the capability reason.
        let text = if snapshot.connected {
            snapshot.version_text.clone()
        } else {
            SharedString::from(crate::localization::text(
                "query.explain_options.disconnected",
            ))
        };
        return div()
            .p(px(s(8.0)))
            .max_w(px(s(320.0)))
            .whitespace_normal()
            .text_size(px(Theme::font_size()))
            .text_color(Theme::text_dim())
            .child(text)
            .into_any_element();
    };
    let opts = snapshot.options;
    let rows = flag_rows(dialect, snapshot.version);

    let mut column = div()
        .flex()
        .flex_col()
        .gap(px(s(6.0)))
        .p(px(s(8.0)))
        .min_w(px(s(240.0)))
        .child(
            div()
                .text_size(px(Theme::font_size()))
                .text_color(Theme::text_dim())
                .child(snapshot.version_text.clone()),
        );

    for (i, row) in rows.iter().enumerate() {
        let checked = flag_checked(&opts, dialect, row.key);
        let key = row.key;
        let enabled = row.enabled;
        let mut box_row = Checkbox::new(("explain-flag", i))
            .label(row.label.clone())
            .checked(checked)
            .disabled(!enabled);
        if enabled {
            let host = host.clone();
            box_row = box_row.on_click(move |value, _window, cx| {
                host.update(cx, |this, cx| {
                    this.set_explain_flag(key, *value, cx);
                });
            });
        }
        let mut row_el = div().id(("explain-flag-row", i)).child(box_row);
        if !enabled && let Some(reason) = row.reason {
            let reason = reason.to_string();
            row_el =
                row_el.app_tooltip(move |_window, cx| crate::app::tooltip_view(reason.clone(), cx));
        }
        column = column.child(row_el);
    }

    let presets = [
        (
            "explain-preset-actual",
            crate::localization::text("query.explain_options.preset_actual"),
            ExplainPreset::Actual,
        ),
        (
            "explain-preset-estimated",
            crate::localization::text("query.explain_options.preset_estimated"),
            ExplainPreset::Estimated,
        ),
        (
            "explain-preset-profile",
            crate::localization::text("query.explain_options.preset_profile"),
            ExplainPreset::Profile,
        ),
    ];
    let mut preset_row = div().flex().flex_row().gap(px(s(6.0)));
    for (id, label, preset) in presets {
        let host = host.clone();
        let unavailable = snapshot.preset_unavailable(preset).map(str::to_string);
        // Presets the engine cannot capture stay visible but disabled, with
        // the plan capability's reason as the tooltip.
        let mut button = Button::new(id)
            .small()
            .label(label)
            .disabled(unavailable.is_some())
            .on_click(move |_, _, cx| {
                host.update(cx, |this, cx| {
                    this.explain_with_preset(preset, cx);
                });
            });
        if let Some(reason) = unavailable {
            button = button.tooltip(reason);
        }
        preset_row = preset_row.child(button);
    }
    column = column.child(preset_row);

    let host_reset = host.clone();
    column
        .child(
            Button::new("explain-reset-defaults")
                .ghost()
                .small()
                .label(crate::localization::text(
                    "query.explain_options.reset_defaults_button",
                ))
                .on_click(move |_, _, cx| {
                    host_reset.update(cx, |this, cx| {
                        this.reset_explain_options(cx);
                    });
                }),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_block_reason_allows_select_and_cte() {
        assert_eq!(profile_block_reason("SELECT 1"), None);
        assert_eq!(
            profile_block_reason("WITH x AS (SELECT 1) SELECT * FROM x"),
            None
        );
    }

    #[test]
    fn profile_block_reason_names_update() {
        let reason = profile_block_reason("UPDATE t SET x = 1").expect("blocked");
        assert!(
            reason.contains("UPDATE"),
            "expected UPDATE in reason, got {reason}"
        );
    }

    #[test]
    fn flag_rows_pg12_disables_wal_generic_memory() {
        let rows = flag_rows(
            PlanDialect::Postgres,
            Some(ServerVersion {
                major: 12,
                minor: 0,
            }),
        );
        assert_eq!(rows.len(), 10);
        let by_key: std::collections::HashMap<_, _> =
            rows.iter().map(|row| (row.key, row)).collect();
        assert!(by_key["settings"].enabled);
        assert!(!by_key["wal"].enabled);
        assert!(!by_key["generic_plan"].enabled);
        assert!(!by_key["memory"].enabled);
        assert_eq!(by_key["wal"].reason, Some("Requires PostgreSQL 13+"));
    }

    #[test]
    fn options_summary_lists_true_flags() {
        let mut opts = ExplainOptions::actual_defaults();
        opts.pg.buffers = true;
        opts.pg.verbose = true;
        assert_eq!(
            options_summary(&opts, PlanDialect::Postgres),
            "ANALYZE, VERBOSE, BUFFERS"
        );
    }
}
