//! Redis Cluster topology pane — every node of the cluster the connection
//! reaches: id, role, the address it advertises, the address SQLLY dials
//! for it (after the node map and any per-node tunnel), its slots, health,
//! replication offset and link state, plus the announced hostname and the
//! TLS server name the client sends (Redis Insight #6537 / #5722 territory:
//! a node that advertises an address this machine cannot reach, or a TLS
//! name that does not match its certificate, shows up here row by row).
//!
//! Trace runs the connection test's staged trace against every node —
//! resolve, tunnel / proxy (with the node map), TCP, TLS, AUTH, HELLO /
//! PING, SELECT, `CLUSTER INFO` — and lists each stage, so the first
//! failing row names what to fix.
//!
//! Read-only: both reports are `SQLLY.*` virtual commands that only read.
//! Query on Node (Redis Insight #2809) opens a new query tab for the
//! connection whose first line is `SQLLY.ON <selected node's address> `,
//! so the command typed after it runs on that node; the pane itself never
//! runs it.

use gpui::{
    Entity, EventEmitter, FocusHandle, IntoElement, ParentElement, Render, Styled, Window, div, px,
};
use gpui_component::Disableable as _;
use gpui_component::button::Button;
use sqlly_core::CellValue;
use sqlly_datatable::SqllyDataTable;

use crate::dialog::{Dialog, DialogFrame};
use crate::theme::Theme;
use crate::tool_dialog::{ToolDialogContext, ToolDialogEvent, ToolQuerySet};

impl EventEmitter<ToolDialogEvent> for RedisClusterTopologyPane {}

/// The node listing the pane shows.
pub const TOPOLOGY_SCRIPT: &str = "SQLLY.TOPOLOGY";
/// The staged per-node connect check.
pub const TRACE_SCRIPT: &str = "SQLLY.TRACE";

/// The start of a query-tab line that runs on one node: `SQLLY.ON
/// host:port ` (the trailing space leaves the caret where the command goes).
pub fn node_query_script(address: &str) -> String {
    format!(
        "{} {address} ",
        sqlly_intel::query::safety::redis::ON_NODE_COMMAND
    )
}

/// The advertised address on row `row` of the node listing.
pub fn node_address(set: &ToolQuerySet, row: usize) -> Option<String> {
    let cells = set.rows.get(row)?;
    text_cell(set, cells, "address").filter(|a| !a.is_empty())
}

/// Number of hash slots a healthy cluster covers.
const SLOT_COUNT: i64 = 16384;

/// What the node listing says about the cluster as a whole.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TopologySummary {
    pub masters: usize,
    pub replicas: usize,
    pub slots_covered: i64,
    /// `address (health, link)` for every node not online and connected.
    pub unhealthy: Vec<String>,
    /// Nodes whose dial address differs from what they advertise.
    pub remapped: usize,
}

fn text_cell(set: &ToolQuerySet, row: &[CellValue], name: &str) -> Option<String> {
    let i = set.columns.iter().position(|c| c.name == name)?;
    match row.get(i)? {
        CellValue::String(s) => Some(s.clone()),
        CellValue::I64(n) => Some(n.to_string()),
        CellValue::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

impl TopologySummary {
    /// Read the `SQLLY.TOPOLOGY` rows by column name.
    pub fn from_set(set: &ToolQuerySet) -> Self {
        let mut out = TopologySummary::default();
        for row in &set.rows {
            let text = |name: &str| text_cell(set, row, name);
            match text("role").as_deref() {
                Some("master") => {
                    out.masters += 1;
                    out.slots_covered += text("slot_count")
                        .and_then(|n| n.parse::<i64>().ok())
                        .unwrap_or(0);
                }
                Some(_) => out.replicas += 1,
                None => {}
            }
            let health = text("health").unwrap_or_default();
            let link = text("link_state");
            let link_bad = link.as_deref().is_some_and(|l| l != "connected");
            if health != "online" || link_bad {
                let address = text("address").unwrap_or_default();
                out.unhealthy.push(match link {
                    Some(link) => format!("{address} ({health}, {link})"),
                    None => format!("{address} ({health})"),
                });
            }
            if let (Some(advertised), Some(dial)) = (text("address"), text("dial_address"))
                && advertised != dial
            {
                out.remapped += 1;
            }
        }
        out
    }

    /// The footer line.
    pub fn describe(&self) -> String {
        let mut text = crate::localization::format(
            "explorer.redis_cluster.slot_summary",
            &[
                ("masters", &self.masters.to_string()),
                ("replicas", &self.replicas.to_string()),
                ("covered", &self.slots_covered.to_string()),
                ("total", &SLOT_COUNT.to_string()),
            ],
        );
        if self.remapped > 0 {
            let key = if self.remapped == 1 {
                "explorer.redis_cluster.remapped_summary_one"
            } else {
                "explorer.redis_cluster.remapped_summary"
            };
            text.push_str(&crate::localization::format(
                key,
                &[("count", &self.remapped.to_string())],
            ));
        }
        if self.unhealthy.is_empty() {
            text.push_str(&crate::localization::text(
                "explorer.redis_cluster.all_online",
            ));
        } else {
            text.push_str(&crate::localization::format(
                "explorer.redis_cluster.not_healthy",
                &[("nodes", &self.unhealthy.join(", "))],
            ));
        }
        text
    }
}

/// The failing stages of a `SQLLY.TRACE` run, `address: stage — detail`.
pub fn trace_failures(set: &ToolQuerySet) -> Vec<String> {
    set.rows
        .iter()
        .filter(|row| text_cell(set, row, "ok").as_deref() == Some("false"))
        .map(|row| {
            format!(
                "{}: {} — {}",
                text_cell(set, row, "address").unwrap_or_default(),
                text_cell(set, row, "stage").unwrap_or_default(),
                text_cell(set, row, "detail").unwrap_or_default()
            )
        })
        .collect()
}

fn pane_title(connection: &str) -> String {
    crate::localization::format(
        "explorer.redis_cluster.title",
        &[("connection", connection)],
    )
}

pub struct RedisClusterTopologyPane {
    context: ToolDialogContext,
    nodes_grid: Option<Entity<SqllyDataTable>>,
    /// The rows behind `nodes_grid`, for Query on Node.
    nodes_set: Option<ToolQuerySet>,
    /// Shown when Query on Node is pressed with no node selected.
    node_hint: Option<String>,
    trace_grid: Option<Entity<SqllyDataTable>>,
    summary: Option<TopologySummary>,
    /// Trace outcome note and whether every stage passed (styled dim rather
    /// than as a warning). The flag is kept beside the text so styling never
    /// depends on comparing translated wording.
    trace_note: Option<(String, bool)>,
    error: Option<String>,
    loading: bool,
    tracing: bool,
    generation: u64,
    focus_handle: FocusHandle,
    frame: DialogFrame,
    windowed: bool,
}

impl RedisClusterTopologyPane {
    pub fn new(context: ToolDialogContext, cx: &mut gpui::Context<Self>) -> Self {
        let mut this = Self {
            context,
            nodes_grid: None,
            nodes_set: None,
            node_hint: None,
            trace_grid: None,
            summary: None,
            trace_note: None,
            error: None,
            loading: true,
            tracing: false,
            generation: 0,
            focus_handle: cx.focus_handle(),
            frame: DialogFrame::new(Theme::scaled(940.0), Theme::scaled(600.0)),
            windowed: false,
        };
        this.refresh(cx);
        this
    }

    /// Point an open pane at another connection.
    pub fn retarget(&mut self, context: ToolDialogContext, cx: &mut gpui::Context<Self>) {
        self.context = context;
        self.nodes_grid = None;
        self.nodes_set = None;
        self.node_hint = None;
        self.trace_grid = None;
        self.summary = None;
        self.trace_note = None;
        self.error = None;
        self.tracing = false;
        self.refresh(cx);
    }

    pub fn profile(&self) -> &sqlly_core::ConnectionProfile {
        &self.context.profile
    }

    fn refresh(&mut self, cx: &mut gpui::Context<Self>) {
        self.loading = true;
        self.generation += 1;
        let generation = self.generation;
        crate::tool_dialog::run_query(
            &self.context,
            TOPOLOGY_SCRIPT.to_string(),
            cx,
            move |this, outcome, cx| {
                if this.generation != generation {
                    return;
                }
                this.loading = false;
                this.error = outcome.error;
                match outcome.sets.first() {
                    Some(set) => {
                        this.summary = Some(TopologySummary::from_set(set));
                        this.nodes_grid = crate::tool_dialog::build_result_grid(set, cx);
                        this.nodes_set = Some(set.clone());
                    }
                    None => {
                        this.summary = None;
                        this.nodes_grid = None;
                        this.nodes_set = None;
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    fn trace(&mut self, cx: &mut gpui::Context<Self>) {
        if self.tracing {
            return;
        }
        self.tracing = true;
        self.trace_note = None;
        let generation = self.generation;
        crate::tool_dialog::run_query(
            &self.context,
            TRACE_SCRIPT.to_string(),
            cx,
            move |this, outcome, cx| {
                if this.generation != generation {
                    return;
                }
                this.tracing = false;
                match (outcome.error, outcome.sets.first()) {
                    (Some(error), _) => this.trace_note = Some((error, false)),
                    (None, Some(set)) => {
                        let failures = trace_failures(set);
                        this.trace_note = Some(if failures.is_empty() {
                            (
                                crate::localization::text(
                                    "explorer.redis_cluster.all_stages_passed",
                                ),
                                true,
                            )
                        } else {
                            (
                                crate::localization::format(
                                    "explorer.redis_cluster.trace_failed",
                                    &[("failures", &failures.join(" · "))],
                                ),
                                false,
                            )
                        });
                        this.trace_grid = crate::tool_dialog::build_result_grid(set, cx);
                    }
                    (None, None) => {
                        this.trace_note = Some((
                            crate::localization::text("explorer.redis_cluster.trace_empty"),
                            false,
                        ));
                    }
                }
                cx.notify();
            },
        );
        cx.notify();
    }

    /// The advertised address of the node selected in the Nodes grid.
    fn selected_node_address(&self, cx: &gpui::App) -> Option<String> {
        let grid = self.nodes_grid.as_ref()?;
        let row = crate::tool_dialog::selected_source_row(grid, cx)?;
        node_address(self.nodes_set.as_ref()?, row)
    }

    /// Open a query tab for this connection that starts with
    /// `SQLLY.ON <selected node> `; the modal pane closes so the tab can be
    /// typed in (a popped-out window stays open).
    fn query_on_node(&mut self, cx: &mut gpui::Context<Self>) {
        let Some(address) = self.selected_node_address(cx) else {
            self.node_hint = Some(crate::localization::text(
                "explorer.redis_cluster.select_node_hint",
            ));
            cx.notify();
            return;
        };
        self.node_hint = None;
        cx.emit(ToolDialogEvent::ScriptToEditorForProfile {
            sql: node_query_script(&address),
            profile: Box::new(self.context.profile.clone()),
        });
        if !self.windowed {
            cx.emit(ToolDialogEvent::Close);
        }
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) {
        if self.windowed {
            window.remove_window();
        } else {
            cx.emit(ToolDialogEvent::Close);
        }
    }

    fn pop_out(&mut self, cx: &mut gpui::Context<Self>) {
        let entity = cx.entity();
        let connection = self.context.profile.name.clone();
        let title = move || pane_title(&connection);
        cx.defer(move |cx| {
            let build_entity = entity.clone();
            let opened = crate::dialog::open_detached(
                crate::dialog::DetachedOptions::localized_with(
                    title,
                    Theme::scaled(940.0),
                    Theme::scaled(660.0),
                ),
                None,
                cx,
                move |_, cx| {
                    build_entity.update(cx, |pane, _| pane.windowed = true);
                    build_entity
                },
            );
            if opened.is_some() {
                entity.update(cx, |_, cx| cx.emit(ToolDialogEvent::Close));
            }
        });
    }

    fn section(
        &self,
        title: String,
        grid: &Option<Entity<SqllyDataTable>>,
        placeholder: &str,
    ) -> gpui::AnyElement {
        let s = Theme::scaled;
        let body = match grid {
            Some(grid) => div().size_full().child(grid.clone().into_any_element()),
            None => div()
                .text_size(px(Theme::font_size_ratio(0.92)))
                .text_color(Theme::text_dim())
                .child(placeholder.to_string()),
        };
        div()
            .flex()
            .flex_col()
            .flex_basis(px(0.0))
            .flex_1()
            .min_h(px(s(90.0)))
            .gap(px(s(4.0)))
            .child(
                div()
                    .text_size(px(Theme::font_size_ratio(0.86)))
                    .text_color(Theme::text())
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .border_1()
                    .border_color(Theme::border())
                    .rounded(px(s(4.0)))
                    .child(body),
            )
            .into_any_element()
    }
}

impl gpui::Focusable for RedisClusterTopologyPane {
    fn focus_handle(&self, _: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RedisClusterTopologyPane {
    fn render(&mut self, window: &mut Window, cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let s = Theme::scaled;
        let focus_handle = self.focus_handle.clone();
        let nodes_placeholder = crate::localization::text(if self.loading {
            "explorer.redis_cluster.nodes_loading"
        } else {
            "explorer.redis_cluster.nodes_empty"
        });
        let trace_placeholder = crate::localization::text(if self.tracing {
            "explorer.redis_cluster.trace_loading"
        } else {
            "explorer.redis_cluster.trace_explanation"
        });
        let footer = match &self.summary {
            Some(summary) => crate::localization::format(
                "explorer.redis_cluster.footer_summary",
                &[("summary", &summary.describe())],
            ),
            None => crate::localization::text("explorer.redis_cluster.footer_initial"),
        };
        let mut dialog = Dialog::new("redis-cluster-topology", |this: &mut Self| &mut this.frame)
            .title(pane_title(&self.context.profile.name))
            .easy_close(true)
            .key_context("RedisClusterTopologyPane")
            .track_focus(&focus_handle)
            .windowed(self.windowed)
            .on_close(|this, window, cx| this.close(window, cx))
            .bare_body()
            .child(
                div()
                    .flex_1()
                    .min_h(px(0.0))
                    .flex()
                    .flex_col()
                    .gap(px(s(8.0)))
                    .p(px(s(14.0)))
                    .children(self.error.clone().map(|err| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.92)))
                            .text_color(Theme::error())
                            .child(err)
                    }))
                    .child(self.section(
                        crate::localization::text("explorer.redis_cluster.nodes_section"),
                        &self.nodes_grid,
                        &nodes_placeholder,
                    ))
                    .children(self.node_hint.clone().map(|hint| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(Theme::warning())
                            .child(hint)
                    }))
                    .child(self.section(
                        crate::localization::text("explorer.redis_cluster.trace_section"),
                        &self.trace_grid,
                        &trace_placeholder,
                    ))
                    .children(self.trace_note.clone().map(|(note, success)| {
                        div()
                            .text_size(px(Theme::font_size_ratio(0.8)))
                            .text_color(if success {
                                Theme::text_dim()
                            } else {
                                Theme::warning()
                            })
                            .child(note)
                    }))
                    .child(
                        div()
                            .text_size(px(Theme::font_size_ratio(0.74)))
                            .text_color(Theme::text_dim())
                            .child(footer),
                    ),
            )
            .leading_button(
                Button::new("redis-topology-refresh")
                    .label(crate::localization::text(
                        "explorer.redis_cluster.refresh_button",
                    ))
                    .disabled(self.loading)
                    .on_click(cx.listener(|this, _, _window, cx| this.refresh(cx))),
            )
            .leading_button(
                Button::new("redis-topology-trace")
                    .label(crate::localization::text(
                        "explorer.redis_cluster.trace_button",
                    ))
                    .disabled(self.tracing || self.loading || self.error.is_some())
                    .on_click(cx.listener(|this, _, _window, cx| this.trace(cx))),
            )
            .leading_button(
                Button::new("redis-topology-query-node")
                    .label(crate::localization::text(
                        "explorer.redis_cluster.query_node_button",
                    ))
                    .disabled(self.nodes_grid.is_none())
                    .on_click(cx.listener(|this, _, _window, cx| this.query_on_node(cx))),
            );
        if !self.windowed {
            dialog = dialog.detachable(|this, _window, cx| this.pop_out(cx));
        }
        dialog.build(self, window, cx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn column(name: &str) -> sqlly_core::ResultColumn {
        sqlly_core::ResultColumn {
            name: name.to_string(),
            data_type: String::new(),
            nullable: true,
        }
    }

    /// The driver's `SQLLY.TOPOLOGY` row for each node of the shared
    /// `CLUSTER NODES` fixture (one master remapped through the node map).
    fn topology_set() -> ToolQuerySet {
        let nodes = sqlly_engines::redis::cluster::parse_cluster_nodes(NODES_FIXTURE);
        let columns = [
            "node_id",
            "role",
            "address",
            "dial_address",
            "slot_count",
            "health",
            "link_state",
        ]
        .map(column)
        .to_vec();
        let rows = nodes
            .iter()
            .map(|n| {
                let dial = if n.address == "10.0.0.1:7000" {
                    "127.0.0.1:17000".to_string()
                } else {
                    n.address.clone()
                };
                vec![
                    CellValue::String(n.id.clone()),
                    CellValue::String(n.role.clone()),
                    CellValue::String(n.address.clone()),
                    CellValue::String(dial),
                    CellValue::I64(n.slot_count()),
                    CellValue::String(n.health.clone()),
                    n.link_state
                        .clone()
                        .map(CellValue::String)
                        .unwrap_or(CellValue::Null),
                ]
            })
            .collect();
        ToolQuerySet { columns, rows }
    }

    pub(super) const NODES_FIXTURE: &str = "\
9e99 10.0.0.2:7001@17001,node-b.example.com master - 0 1792441110000 2 connected 5461-10922
50bb 10.0.0.5:7004@17004 slave,pfail f7dd 0 1792441110771 3 connected
1017 10.0.0.6:7005@17005 slave 5165 0 1792441111274 1 disconnected
f7dd 10.0.0.3:7002@17002 master - 0 1792441109765 3 connected 10923-16383
1d78 10.0.0.4:7003@17003 slave 9e99 0 1792441110268 2 connected
5165 10.0.0.1:7000@17000 myself,master - 0 1792441111000 1 connected 0-5460
";

    #[test]
    fn redis_cluster_topology_summary_counts_roles_slots_and_trouble() {
        let summary = TopologySummary::from_set(&topology_set());
        assert_eq!(summary.masters, 3);
        assert_eq!(summary.replicas, 3);
        assert_eq!(summary.slots_covered, SLOT_COUNT);
        assert_eq!(summary.remapped, 1);
        assert_eq!(
            summary.unhealthy,
            vec![
                "10.0.0.6:7005 (online, disconnected)".to_string(),
                "10.0.0.5:7004 (suspected, connected)".to_string(),
            ]
        );
        let text = summary.describe();
        assert!(text.contains("3 masters, 3 replicas"), "{text}");
        assert!(text.contains("16384 of 16384 slots covered"), "{text}");
        assert!(text.contains("1 node dialed at another address"), "{text}");
        assert!(text.contains("not healthy: 10.0.0.6:7005"), "{text}");
        let plural = TopologySummary {
            remapped: 2,
            ..summary.clone()
        }
        .describe();
        assert!(
            plural.contains("2 nodes dialed at another address"),
            "{plural}"
        );
    }

    #[test]
    fn redis_cluster_topology_trace_failures_name_node_and_stage() {
        let set = ToolQuerySet {
            columns: ["node_id", "address", "stage", "ok", "detail"]
                .map(column)
                .to_vec(),
            rows: vec![
                vec![
                    CellValue::String("a".into()),
                    CellValue::String("10.0.0.1:7000".into()),
                    CellValue::String("dial".into()),
                    CellValue::Bool(true),
                    CellValue::String("ok".into()),
                ],
                vec![
                    CellValue::String("b".into()),
                    CellValue::String("10.0.0.2:7001".into()),
                    CellValue::String("connect".into()),
                    CellValue::Bool(false),
                    CellValue::String("Connection refused".into()),
                ],
            ],
        };
        assert_eq!(
            trace_failures(&set),
            vec!["10.0.0.2:7001: connect — Connection refused".to_string()]
        );
    }

    /// Query on Node starts the selected row's line with `SQLLY.ON <its
    /// advertised address> `, which the editor then runs on that node; the
    /// prefix alone changes nothing, and a write typed after it is still a
    /// write for the safety gates.
    #[test]
    fn redis_cluster_topology_query_on_node_targets_the_selected_address() {
        let set = topology_set();
        let replica = set
            .rows
            .iter()
            .position(|r| text_cell(&set, r, "role").as_deref() == Some("replica"))
            .expect("a replica row");
        let address = node_address(&set, replica).expect("address");
        let script = node_query_script(&address);
        assert_eq!(script, format!("SQLLY.ON {address} "));
        assert!(node_address(&set, set.rows.len()).is_none());
        use sqlly_intel::query::safety::redis::{CommandAccess, command_access, parse_script};
        let typed = parse_script(&format!("{script}INFO replication")).expect("parses");
        let (target, inner) = typed[0].on_node().expect("wrapped");
        assert_eq!(target, address);
        assert_eq!(inner.name(), "INFO");
        assert_eq!(command_access(&typed[0]), CommandAccess::ReadOnly);
        let flush = parse_script(&format!("{script}FLUSHALL")).expect("parses");
        assert_eq!(command_access(&flush[0]), CommandAccess::Destructive);
        let analysis = crate::mutation_review::analyze_redis(&format!("{script}FLUSHALL"))
            .expect("FLUSHALL needs review");
        assert_eq!(analysis.statements.len(), 1);
        assert!(!analysis.statements[0].has_where_clause, "wholesale delete");
    }

    #[test]
    fn redis_cluster_topology_scripts_are_read_only_virtual_commands() {
        for script in [TOPOLOGY_SCRIPT, TRACE_SCRIPT] {
            let commands = sqlly_intel::query::safety::redis::parse_script(script).expect("parses");
            assert_eq!(commands.len(), 1);
            assert_eq!(
                sqlly_intel::query::safety::redis::command_access(&commands[0]),
                sqlly_intel::query::safety::redis::CommandAccess::ReadOnly,
                "{script}"
            );
        }
    }
}
