use crate::tooltip::AppTooltip;
use gpui::{
    Animation, AnimationExt as _, Context, ElementId, IntoElement, MouseDownEvent, Render, Styled,
    Window, anchored, deferred, div, img, point, prelude::*, px,
};
use gpui_component::Icon;
use sqlly_core::ConnectionProfile;
use std::time::Duration;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;
use uuid::Uuid;
#[cfg(target_arch = "wasm32")]
use web_time::Instant;

use super::load::{LoadState, ServerHealth, extract_db_from_id, sanitize_display, server_health};
use super::model::{NodeKind, TreeNode, find_node_any, parent_id_of, server_uuid_from_id};
use super::{DraggedIdentifier, SchemaTree, SchemaTreeEvent, TREE_LEVEL_INDENT, TREE_ROW_INSET};
use crate::theme::{Theme, themed_sidebar_row};
use std::rc::Rc;
// Imports used only by the test module below.
#[cfg(test)]
use super::ExplorerMenuAction;
#[cfg(test)]
use sqlly_services::Services;

#[derive(Clone)]
pub(crate) struct FlatNode {
    pub(crate) id: gpui::SharedString,
    pub(crate) depth: usize,
    pub(crate) title: gpui::SharedString,
    /// Lowercased `title`, cloned from the node so filter highlighting does
    /// not `to_lowercase` per visible row.
    pub(crate) title_lower: gpui::SharedString,
    pub(crate) detail: Option<gpui::SharedString>,
    pub(crate) kind: NodeKind,
    pub(crate) expanded: bool,
    pub(crate) load_state: LoadState,
    /// How many children this node has loaded. Rendered as a count on folder
    /// rows: a folder that has loaded and found nothing looked exactly like a
    /// folder whose load quietly finished, right down to keeping its ▾ over
    /// empty space.
    pub(crate) child_count: usize,
    /// Column-alignment context when this node is a column/parameter row
    /// under an expanded object: widths (in characters) of the widest sibling
    /// name and data type, plus which alignment preference governs it.
    pub(crate) align: Option<AlignInfo>,
    /// Server rows only: the environment identity color (raw `#RRGGBB`),
    /// carried through flattening so the rail can paint the dot per frame.
    pub(crate) env_color_hex: Option<gpui::SharedString>,
    /// Hover group name (`row:{id}`), cloned from the node.
    pub(crate) row_group: gpui::SharedString,
    pub(crate) tier: Option<sqlly_intel::intellisense::MatchTier>,
}

pub(crate) fn flat_from_node(
    node: &TreeNode,
    expanded: bool,
    align: Option<AlignInfo>,
) -> FlatNode {
    let is_meta_row = matches!(node.kind, NodeKind::Column | NodeKind::Parameter { .. });
    FlatNode {
        id: gpui::SharedString::from(node.id.as_str()),
        depth: node.depth,
        title: gpui::SharedString::from(node.title.as_str()),
        title_lower: gpui::SharedString::from(node.title_lower.as_str()),
        detail: node.detail.as_deref().map(gpui::SharedString::from),
        kind: node.kind.clone(),
        expanded,
        load_state: node.load_state.clone(),
        child_count: node.children.len(),
        align: if is_meta_row { align } else { None },
        env_color_hex: node.env_color_hex.as_deref().map(gpui::SharedString::from),
        row_group: node.row_group.clone(),
        tier: None,
    }
}

fn flat_from_node_tier(
    node: &TreeNode,
    expanded: bool,
    align: Option<AlignInfo>,
    tier: Option<sqlly_intel::intellisense::MatchTier>,
) -> FlatNode {
    let mut flat = flat_from_node(node, expanded, align);
    flat.tier = tier;
    flat
}

#[derive(Clone, Copy)]
pub(crate) struct AlignInfo {
    pub(crate) name_chars: usize,
    pub(crate) type_chars: usize,
    pub(crate) surface: crate::preferences::AlignSurface,
}

/// How much of a column row's metadata the rail is currently wide enough to
/// show. When space runs short the columns are shed in a fixed priority order
/// — identity first, then nullability, then the data type itself — so metadata
/// never rides up on top of the column names. If the name alone can't fit, all
/// three go (`None`) and the name ellipsizes to keep the row.
#[derive(Clone, Copy, PartialEq)]
pub(crate) enum MetaLevel {
    All,
    NoIdentity,
    TypeOnly,
    None,
}

/// Alignment context for `parent`'s children: max name/type widths across
/// the column/parameter siblings, so each row can pad its name and type into
/// vertical columns. `None` for parents whose children aren't metadata rows.
pub(crate) fn align_info_for(parent: &TreeNode) -> Option<AlignInfo> {
    use crate::preferences::AlignSurface;
    let surface = match parent.kind {
        NodeKind::Table { .. } => AlignSurface::TableColumns,
        NodeKind::View { .. } => AlignSurface::ViewColumns,
        NodeKind::Procedure { .. } => AlignSurface::ProcedureParameters,
        NodeKind::Function { .. } => AlignSurface::FunctionParameters,
        _ => return None,
    };
    let mut name_chars = 0usize;
    let mut type_chars = 0usize;
    for child in &parent.children {
        if !matches!(child.kind, NodeKind::Column | NodeKind::Parameter { .. }) {
            continue;
        }
        name_chars = name_chars.max(child.title.chars().count());
        if let Some(detail) = &child.detail {
            let dtype = detail.split(", ").next().unwrap_or(detail);
            type_chars = type_chars.max(dtype.chars().count());
        }
    }
    (name_chars > 0).then_some(AlignInfo {
        name_chars,
        type_chars,
        surface,
    })
}

/// Extract the database names from a set of root nodes (the roots are the
/// discovered databases). Non-database roots and folders are skipped. Used by
/// [`SchemaTree::database_names`] and tested directly so the query-bar database
/// dropdown has a covered backing function.
pub(crate) fn database_names_from_roots(roots: &[TreeNode]) -> Vec<String> {
    roots
        .iter()
        .filter_map(|n| match &n.kind {
            NodeKind::Database(name) => Some(name.clone()),
            _ => None,
        })
        .collect()
}

/// Rows painted on first layout, before the scroll viewport has a height.
/// One screen of typical explorer rows — never the full flatten, which would
/// build thousands of elements on first paint.
pub(crate) const FIRST_PAINT_ROW_CAP: usize = 40;

/// Inclusive-exclusive window of flatten indices to materialize as row
/// elements. When `viewport_h == 0` (first layout), caps at
/// [`FIRST_PAINT_ROW_CAP`] instead of `visible_len`.
pub(crate) fn viewport_window(
    visible_len: usize,
    viewport_h: f32,
    offset_y: f32,
    row_h: f32,
) -> (usize, usize) {
    if visible_len == 0 {
        return (0, 0);
    }
    if viewport_h > 0.0 && row_h > 0.0 {
        let scroll_y = (-offset_y).max(0.0);
        let first = (scroll_y / row_h).floor() as usize;
        let count = (viewport_h / row_h).ceil() as usize + 6;
        let start = first.saturating_sub(2).min(visible_len);
        let end = start.saturating_add(count).min(visible_len);
        (start, end)
    } else {
        (0, FIRST_PAINT_ROW_CAP.min(visible_len))
    }
}

/// Flatten the tree into a list of visible nodes for rendering.
pub(crate) fn flatten_tree(nodes: &[TreeNode], out: &mut Vec<FlatNode>) {
    flatten_tree_with(nodes, out, None);
}

pub(crate) fn flatten_tree_with(
    nodes: &[TreeNode],
    out: &mut Vec<FlatNode>,
    align: Option<AlignInfo>,
) {
    for node in nodes {
        out.push(flat_from_node(node, node.expanded, align));
        if node.expanded {
            flatten_tree_with(&node.children, out, align_info_for(node));
        }
    }
}

// Test-only counter of `node_matches` calls, so a regression test can prove the
// filter does O(N) match work (exactly one comparison per node), not the
// O(N·depth) of the old repeated-subtree-scan implementation.
#[cfg(test)]
thread_local! {
    pub(crate) static NODE_MATCH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static FLATTEN_CACHE_BUILDS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static FLATTEN_CACHE_HITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Whether a node's own label matches the compiled query.
pub(crate) fn node_match_tier(
    node: &TreeNode,
    q: &crate::explorer_search::CompiledQuery,
) -> Option<sqlly_intel::intellisense::MatchTier> {
    #[cfg(test)]
    NODE_MATCH_CALLS.with(|c| c.set(c.get() + 1));
    if !q.allows_kind(&node.kind) {
        return None;
    }
    if let Some(tier) = q.match_title(&node.title, &node.title_lower) {
        return Some(tier);
    }
    if q.kind_only() && q.is_scoped_object(&node.kind) {
        return Some(sqlly_intel::intellisense::MatchTier::Exact);
    }
    None
}

/// Flatten only the nodes on a path to a match, revealing them regardless of
/// their stored expansion state — a filter that made you expand folders to find
/// the hit would not be doing the job. Returns whether any node in `nodes` (or
/// their loaded descendants) matched, so the caller learns of a descendant match
/// without a second walk.
///
/// This is a single post-order pass: each node's title is matched exactly once,
/// and the recursion into a node's children doubles as its "does anything below
/// me match" answer. The earlier version asked `subtree_matches` for every node
/// and then recursed, re-walking each subtree once per ancestor — O(N²) work
/// (and one `to_lowercase` per visit) on every keystroke over a large loaded
/// tree. This is O(N).
///
/// "Beneath" means *loaded* children only: the tree fetches lazily, so a filter
/// can only narrow what has been read from the server. Typing `acc` under a
/// 374-table folder you have opened finds those tables; it never enumerates a
/// folder you have not expanded.
pub(crate) fn flatten_tree_filtered(
    nodes: &[TreeNode],
    out: &mut Vec<FlatNode>,
    align: Option<AlignInfo>,
    q: &crate::explorer_search::CompiledQuery,
) -> bool {
    struct Emitted {
        orig: usize,
        rankable: bool,
        tier: Option<sqlly_intel::intellisense::MatchTier>,
        rows: Vec<FlatNode>,
    }
    let mut items: Vec<Emitted> = Vec::new();
    for (orig, node) in nodes.iter().enumerate() {
        let tier = node_match_tier(node, q);
        let self_matches = tier.is_some();
        let reveal_all = self_matches && crate::explorer_search::is_reveal_all_kind(&node.kind);

        if reveal_all {
            let mut rows = vec![flat_from_node_tier(node, true, align, tier)];
            flatten_tree_revealed(&node.children, &mut rows, align_info_for(node));
            items.push(Emitted {
                orig,
                rankable: crate::explorer_search::is_rankable_object(&node.kind),
                tier,
                rows,
            });
            continue;
        }

        let mut child_rows = Vec::new();
        let has_match_below =
            flatten_tree_filtered(&node.children, &mut child_rows, align_info_for(node), q);
        if !self_matches && !has_match_below {
            continue;
        }
        let mut rows = vec![flat_from_node_tier(
            node,
            has_match_below || node.expanded,
            align,
            tier,
        )];
        rows.append(&mut child_rows);
        items.push(Emitted {
            orig,
            rankable: crate::explorer_search::is_rankable_object(&node.kind),
            tier,
            rows,
        });
    }
    items.sort_by_key(|e| {
        if e.rankable {
            (0u8, e.tier.map(|t| t as u8).unwrap_or(99), e.orig)
        } else {
            (1u8, 0, e.orig)
        }
    });
    let any_matched = !items.is_empty();
    for item in items {
        out.extend(item.rows);
    }
    any_matched
}

/// Flatten every loaded descendant of a filter-matched node, ignoring stored
/// expansion state. Used so a table (or any node) that matches the filter shows
/// its full contents rather than only the children that also match.
pub(crate) fn flatten_tree_revealed(
    nodes: &[TreeNode],
    out: &mut Vec<FlatNode>,
    align: Option<AlignInfo>,
) {
    for node in nodes {
        out.push(flat_from_node(node, !node.children.is_empty(), align));
        flatten_tree_revealed(&node.children, out, align_info_for(node));
    }
}

/// Render a node title with the active filter match highlighted. `needle` is
/// already trimmed and lower-cased; `title_lower` is the node's stored
/// lowercased title. An empty needle (or a non-ASCII title whose lower-cased
/// byte length differs) renders the plain title so nothing is mis-sliced.
pub(crate) fn highlighted_title(
    title: &str,
    title_lower: &str,
    q: &crate::explorer_search::CompiledQuery,
) -> gpui::AnyElement {
    if let Some(range) = q.highlight_range(title, title_lower)
        && let (Some(before), Some(matched), Some(after)) = (
            title.get(..range.start),
            title.get(range.start..range.end),
            title.get(range.end..),
        )
    {
        return div()
            .flex()
            .flex_row()
            .items_center()
            .when(!before.is_empty(), |d| d.child(before.to_string()))
            .child(
                div()
                    .px(px(1.0))
                    .rounded(px(2.0))
                    .bg(Theme::accent().opacity(0.30))
                    .text_color(Theme::text())
                    .child(matched.to_string()),
            )
            .when(!after.is_empty(), |d| d.child(after.to_string()))
            .into_any_element();
    }
    title.to_string().into_any_element()
}

/// Async load children for a node based on its kind.
/// The databases SQL Server ships with, for the system-database placement
/// preference.
pub(crate) fn object_tooltip(
    kind: &NodeKind,
    title: &str,
    detail: Option<&str>,
    node_id: &str,
) -> String {
    let db = extract_db_from_id(node_id);
    // Built into one buffer rather than a `Vec` + `join`. This runs for every
    // visible row on every frame, so it is the row's baseline cost, not an
    // occasional one.
    let qualified = |schema: &str, name: &str| -> String {
        let mut s = String::with_capacity(db.len() + schema.len() + name.len() + 2);
        if !db.is_empty() {
            s.push_str(&sanitize_display(&db));
            s.push('.');
        }
        if !schema.is_empty() {
            s.push_str(&sanitize_display(schema));
            s.push('.');
        }
        s.push_str(&sanitize_display(name));
        s
    };

    let text = match kind {
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name } => qualified(schema, name),
        _ => title.to_string(),
    };

    // Kinds whose rows draw no gutter glyph ([`NodeKind::glyph_icon`] is
    // `None`) look identical to each other once the legacy letter markers were
    // retired — the tooltip is the remaining carrier of what the row *is*, so
    // it spells the kind out. Glyphed kinds (tables, views, procedures,
    // functions, columns, indexes) and folders (which name their kind in the
    // label) stay as they were. Views / procedures / functions still get a
    // kind word here because the tooltip is the full qualified name plus kind.
    let kind_label = match kind {
        NodeKind::View { .. } => Some("explorer.schema_render.kind_view"),
        NodeKind::Procedure { .. } => Some("explorer.schema_render.kind_procedure"),
        NodeKind::Function { .. } => Some("explorer.schema_render.kind_function"),
        NodeKind::Type { .. } => Some("explorer.schema_render.kind_type"),
        NodeKind::Statistic { .. } => Some("explorer.schema_render.kind_statistic"),
        NodeKind::RedisKeyFolder { .. } => Some("explorer.schema_render.kind_key_prefix"),
        NodeKind::RedisIndex { .. } => Some("explorer.schema_render.kind_search_index"),
        _ => None,
    };
    // A key's tooltip spells out its type and exact TTL; the row shows a
    // compact form of the same in its detail.
    if let NodeKind::RedisKey {
        kind, ttl_seconds, ..
    } = kind
    {
        return if let Some(seconds) = ttl_seconds {
            crate::localization::format(
                "explorer.schema_render.redis_key_expiring_tooltip",
                &[
                    ("title", &text),
                    ("kind", kind),
                    ("seconds", &seconds.to_string()),
                ],
            )
        } else {
            crate::localization::format(
                "explorer.schema_render.redis_key_no_expiry_tooltip",
                &[("title", &text), ("kind", kind)],
            )
        };
    }

    let detail = detail.filter(|d| !d.is_empty()).map(str::to_string);
    match (kind_label, detail) {
        (Some(kind), Some(detail)) => crate::localization::format(
            "explorer.schema_render.tooltip_kind_detail",
            &[
                ("title", &text),
                ("kind", &crate::localization::text(kind)),
                ("detail", &detail),
            ],
        ),
        (Some(kind), None) => crate::localization::format(
            "explorer.schema_render.tooltip_kind",
            &[("title", &text), ("kind", &crate::localization::text(kind))],
        ),
        (None, Some(detail)) => crate::localization::format(
            "explorer.schema_render.tooltip_detail",
            &[("title", &text), ("detail", &detail)],
        ),
        (None, None) => text,
    }
}

/// The failed-load marker is a status dot, never the word "failed".
pub(crate) fn failed_load_inline_word() -> Option<&'static str> {
    None
}

fn localized_column_attributes(attributes: &str) -> String {
    attributes
        .split(", ")
        .map(|attribute| {
            crate::localization::text(match attribute {
                "identity" => "explorer.schema_render.column_identity",
                "not null" => "explorer.schema_render.column_not_null",
                "nullable" => "explorer.schema_render.column_nullable",
                _ => return attribute.to_string(),
            })
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Append why a row's load failed to whatever its tooltip already says.
///
/// Applied to *every* row's tooltip, whatever its base. It used to live inside
/// the object tooltip, which meant server rows — the ones that actually fail,
/// because they are where connecting happens — silently dropped it: the row said
/// "failed", the tooltip offered "Click to try again", and the reason existed
/// nowhere the user could reach.
pub(crate) fn with_load_state(text: String, load_state: &LoadState) -> gpui::SharedString {
    match load_state {
        LoadState::Failed(error) => crate::localization::format(
            "explorer.schema_render.load_failed_tooltip",
            &[("text", &text), ("error", error)],
        ),
        LoadState::PermissionDenied(error) => crate::localization::format(
            "explorer.schema_render.permission_denied_tooltip",
            &[("text", &text), ("error", error)],
        ),
        _ => text,
    }
    .into()
}

/// What a server row's tooltip says: where it points, who it connects as, and
/// whether it is connected right now.
///
/// The row itself can only afford a name, and the status bar only ever
/// describes the *active* connection — so for every other server in the rail,
/// this is the only place the host and auth mode exist.
pub(crate) fn server_tooltip(
    profile: &ConnectionProfile,
    conn_state: crate::schema_tree::ConnectionState,
) -> String {
    use sqlly_core::AuthenticationType as Auth;
    let auth = match &profile.authentication {
        Auth::SqlLogin { username, .. } => crate::localization::format(
            "explorer.schema_render.auth_sql_login",
            &[("username", username)],
        ),
        Auth::EntraDeviceCode | Auth::EntraBrowser => profile
            .entra_account
            .as_ref()
            .and_then(|a| a.username.clone())
            .map(|username| {
                crate::localization::format(
                    "explorer.schema_render.auth_entra_user",
                    &[("username", &username)],
                )
            })
            .unwrap_or_else(|| crate::localization::text("explorer.schema_render.auth_entra")),
        Auth::Integrated => crate::localization::text("explorer.schema_render.auth_integrated"),
        Auth::AwsRdsIam {
            username, region, ..
        } => crate::localization::format(
            "explorer.schema_render.auth_aws_iam",
            &[("username", username), ("region", region)],
        ),
        Auth::GcpCloudSqlIam { username } => crate::localization::format(
            "explorer.schema_render.auth_google_iam",
            &[("username", username)],
        ),
        Auth::AuthToken { .. } => crate::localization::text("explorer.schema_render.auth_token"),
    };
    let mut lines = vec![
        profile.name.clone(),
        format!("{}:{}", profile.server, profile.port),
    ];
    if let Some(database) = &profile.database {
        lines.push(crate::localization::format(
            "explorer.schema_render.database_line",
            &[("database", database)],
        ));
    }
    lines.push(crate::localization::format(
        "explorer.schema_render.auth_line",
        &[("auth", &auth)],
    ));
    lines.push(crate::localization::format(
        "explorer.schema_render.environment_line",
        &[(
            "environment",
            &crate::status_bar::environment_label(profile.environment),
        )],
    ));
    use crate::schema_tree::ConnectionState;
    let last_known_at = crate::connection_health::relative_time(
        profile
            .status
            .last_tested_at
            .or(profile.status.last_connected_at),
        chrono::Utc::now(),
    );
    let state = match conn_state {
        ConnectionState::Connected => {
            crate::localization::text("explorer.schema_render.state_connected")
        }
        ConnectionState::Unreachable => {
            crate::localization::text("explorer.schema_render.state_unreachable")
        }
        ConnectionState::StaleConnected => crate::localization::format(
            "explorer.schema_render.state_stale_connected",
            &[("last_known_at", &last_known_at)],
        ),
        ConnectionState::StaleUnreachable => crate::localization::format(
            "explorer.schema_render.state_stale_unreachable",
            &[("last_known_at", &last_known_at)],
        ),
        ConnectionState::Disconnected => {
            crate::localization::text("explorer.schema_render.state_disconnected")
        }
    };
    lines.push(state);
    if let Some(notes) = profile
        .notes
        .as_deref()
        .map(str::trim)
        .filter(|notes| !notes.is_empty())
    {
        lines.push(crate::localization::format(
            "explorer.schema_render.notes_line",
            &[("notes", notes)],
        ));
    }
    lines.join("\n")
}

/// Whether a node is a container whose child count is worth stating.
///
/// Tables/Views/Programmability answer "how much is in here?" — the question you
/// ask before deciding whether to open one. Servers, databases, and schema
/// folders don't: their child count is a list of databases or a fixed set of
/// standard folders, which the user can already see.
pub(crate) fn is_folder_kind(kind: &NodeKind) -> bool {
    matches!(
        kind,
        NodeKind::TablesFolder
            | NodeKind::ViewsFolder
            | NodeKind::ProgrammabilityFolder
            | NodeKind::SystemDatabasesFolder
            | NodeKind::SystemSchemasFolder
            | NodeKind::EmptySchemasFolder
            | NodeKind::PinnedFolder { .. }
            | NodeKind::RedisKeyFolder { .. }
    )
}

/// The drop shadow painted under the explorer's context menu.
///
/// Two layers, the shape shadcn's `shadow-xl` uses: a wide, soft one that lifts
/// the whole menu off the tree, and a tighter one that keeps the near edge from
/// floating. Deeper in dark mode, where the menu surface and the sidebar behind
/// it are close in value and only the shadow separates them.
pub(crate) fn context_menu_shadow() -> Vec<gpui::BoxShadow> {
    use gpui::hsla;

    // Asked of the canvas rather than the theme *kind*, so every palette —
    // including the light ones inside a "dark" family — gets the ink its own
    // background needs.
    let dark = Theme::bg().l < 0.5;
    let (wide, tight) = if dark { (0.55, 0.40) } else { (0.28, 0.18) };
    vec![
        gpui::BoxShadow {
            color: hsla(0., 0., 0., wide),
            offset: point(px(0.), px(Theme::scaled(12.0))),
            blur_radius: px(Theme::scaled(24.0)),
            spread_radius: px(Theme::scaled(-6.0)),
            inset: false,
        },
        gpui::BoxShadow {
            color: hsla(0., 0., 0., tight),
            offset: point(px(0.), px(Theme::scaled(4.0))),
            blur_radius: px(Theme::scaled(8.0)),
            spread_radius: px(Theme::scaled(-2.0)),
            inset: false,
        },
    ]
}

/// The color of a node's kind marker.
///
/// Servers are deliberately *not* accent-colored. The accent means one thing in
/// this tree — "this row is selected" — and an accent parked permanently on
/// every server icon meant the selection had to compete with a dozen static
/// accents to be found. Connection state is carried instead by the icon turning
/// [`Theme::success`] on the connected server, the same green the status bar's
/// connection dot uses.
pub(crate) fn kind_color(kind: &NodeKind) -> gpui::Hsla {
    match kind {
        NodeKind::Server { .. } => Theme::text_dim(),
        NodeKind::Group { .. } => Theme::text_dim(),
        NodeKind::Database(_) => Theme::kind_database(),
        // These three were one appearance for three concepts: all "SYS", all
        // muted. Each now reads as what it holds — a folder of databases, a
        // folder of schemas, a folder of *empty* schemas — so the marker, the
        // color, and the label agree instead of the label doing all the work.
        NodeKind::SystemDatabasesFolder => Theme::kind_database(),
        NodeKind::SystemSchemasFolder => Theme::kind_schema(),
        // Muted on purpose: an empty schema is the one of the three with
        // nothing in it, and that is worth saying quietly rather than not at all.
        NodeKind::EmptySchemasFolder => Theme::text_dim(),
        NodeKind::SchemaFolder { .. } => Theme::kind_schema(),
        NodeKind::TablesFolder | NodeKind::Table { .. } | NodeKind::PinnedFolder { .. } => {
            Theme::kind_table()
        }
        NodeKind::ViewsFolder | NodeKind::View { .. } => Theme::kind_view(),
        NodeKind::ProgrammabilityFolder => Theme::kind_procedure(),
        NodeKind::Procedure { .. } => Theme::kind_procedure(),
        NodeKind::Function { .. } => Theme::kind_function(),
        NodeKind::IndexesFolder { .. } | NodeKind::Index { .. } => Theme::kind_column(),
        NodeKind::StatisticsFolder { .. } | NodeKind::Statistic { .. } => Theme::kind_column(),
        NodeKind::ForeignKeysFolder { .. } | NodeKind::ForeignKey { .. } => Theme::kind_view(),
        NodeKind::TypesFolder | NodeKind::Type { .. } => Theme::kind_procedure(),
        NodeKind::Column => Theme::kind_column(),
        NodeKind::Parameter { .. } => Theme::kind_column(),
        NodeKind::Error => Theme::error(),
        NodeKind::AzureAccount => Theme::text_dim(),
        NodeKind::AzureSubscription => Theme::text_dim(),
        NodeKind::AzureSqlServer => Theme::text_dim(),
        NodeKind::AzureDatabase(_) => Theme::kind_database(),
        NodeKind::Pinned { .. } => Theme::kind_table(),
        _ => Theme::text_dim(),
    }
}
struct DragPreview {
    label: String,
}

impl Render for DragPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py_1()
            .bg(Theme::popup_bg())
            .border_1()
            .border_color(Theme::popup_border())
            .text_color(Theme::text())
            .child(self.label.clone())
    }
}

fn dragged_identifier_for(
    roots: &[TreeNode],
    node_id: &str,
    kind: &NodeKind,
) -> Option<DraggedIdentifier> {
    let server_id = server_uuid_from_id(node_id);
    let database = {
        let db = extract_db_from_id(node_id);
        if db.is_empty() { None } else { Some(db) }
    };
    match kind {
        NodeKind::Table { schema, name }
        | NodeKind::View { schema, name }
        | NodeKind::MaterializedView { schema, name }
        | NodeKind::Procedure { schema, name }
        | NodeKind::Function { schema, name }
        | NodeKind::Sequence { schema, name }
        | NodeKind::Synonym { schema, name, .. }
        | NodeKind::Event { schema, name } => Some(DraggedIdentifier {
            server_id,
            database,
            schema: Some(schema.clone()).filter(|s| !s.is_empty()),
            object: name.clone(),
            column: None,
        }),
        NodeKind::Extension { name, .. } => Some(DraggedIdentifier {
            server_id,
            database,
            schema: None,
            object: name.clone(),
            column: None,
        }),
        NodeKind::Trigger { schema, name, .. } | NodeKind::Constraint { schema, name, .. } => {
            Some(DraggedIdentifier {
                server_id,
                database,
                schema: Some(schema.clone()).filter(|s| !s.is_empty()),
                object: name.clone(),
                column: None,
            })
        }
        NodeKind::Column | NodeKind::Parameter { .. } => {
            let parent_id = parent_id_of(roots, node_id)?;
            let parent = find_node_any(roots, &parent_id)?;
            let (schema, object) = match &parent.kind {
                NodeKind::Table { schema, name }
                | NodeKind::View { schema, name }
                | NodeKind::MaterializedView { schema, name }
                | NodeKind::Procedure { schema, name }
                | NodeKind::Function { schema, name } => (schema.clone(), name.clone()),
                _ => return None,
            };
            let column = find_node_any(roots, node_id)?.title.clone();
            Some(DraggedIdentifier {
                server_id,
                database,
                schema: Some(schema).filter(|s| !s.is_empty()),
                object,
                column: Some(column),
            })
        }
        _ => None,
    }
}

impl SchemaTree {
    /// The bulk-operations banner pinned above the rows while a multi-selection
    /// is active: the count, the four review-first actions, and a clear
    /// affordance. Every action emits through the tree's own event, gathers the
    /// selection at click time, and consumes it.
    fn render_bulk_banner(&self, cx: &mut Context<Self>) -> gpui::Div {
        use gpui_component::button::{Button, ButtonVariants as _};
        use gpui_component::{Disableable as _, Sizable as _};

        let s = Theme::scaled;
        let count = self.multi.len();
        let chip = |id: &'static str, label: &'static str, enabled: bool| -> Button {
            Button::new(id)
                .label(crate::localization::text(label))
                .xsmall()
                .ghost()
                .disabled(!enabled)
        };

        // Maintenance lists the first selected object's engine vocabulary;
        // an engine without one (or no selection yet) disables the chip.
        let maintenance_ok = self
            .multi
            .members()
            .first()
            .and_then(|id| server_uuid_from_id(id))
            .and_then(|sid| {
                sqlly_services::tool_dialects::maintenance_dialect(
                    self.database_type_for(Some(sid)),
                )
            })
            .is_some_and(|dialect| {
                !sqlly_services::maintenance::ops_for(
                    dialect,
                    sqlly_services::maintenance::MaintenanceScope::Table,
                )
                .is_empty()
            });
        // The export handoff opens the native table-export dialog.
        let export_ok = !cfg!(target_arch = "wasm32");

        let mut tint = Theme::accent();
        tint.a = 0.09;
        div()
            .flex()
            .flex_col()
            .gap(px(s(4.0)))
            .px(px(s(8.0)))
            .py(px(s(6.0)))
            .bg(tint)
            .border_b_1()
            .border_color(Theme::border())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .text_color(Theme::text())
                            .child(if count == 1 {
                                crate::localization::text(
                                    "explorer.schema_render.bulk_selection_one",
                                )
                            } else {
                                crate::localization::format(
                                    "explorer.schema_render.bulk_selection_count",
                                    &[("count", &count.to_string())],
                                )
                            }),
                    )
                    .child(
                        Button::new("bulk-clear")
                            .icon(gpui_component::IconName::Close)
                            .xsmall()
                            .ghost()
                            .tooltip(crate::localization::text(
                                "explorer.schema_render.bulk_clear_tooltip",
                            ))
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.multi.clear();
                                cx.notify();
                            })),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .items_center()
                    .gap(px(s(4.0)))
                    .child(
                        chip(
                            "bulk-create",
                            "explorer.schema_render.bulk_script_create",
                            true,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.emit_bulk_action(super::BulkTreeAction::ScriptCreate, cx);
                        })),
                    )
                    .child(
                        chip("bulk-drop", "explorer.schema_render.bulk_script_drop", true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.emit_bulk_action(super::BulkTreeAction::ScriptDrop, cx);
                            })),
                    )
                    .child(
                        chip(
                            "bulk-export",
                            "explorer.schema_render.bulk_export_data",
                            export_ok,
                        )
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.emit_bulk_action(super::BulkTreeAction::ExportData, cx);
                        })),
                    )
                    .child(
                        chip(
                            "bulk-maintenance",
                            "explorer.schema_render.bulk_maintenance",
                            maintenance_ok,
                        )
                        .on_click(cx.listener(|this, _, window, cx| {
                            let position = window.mouse_position();
                            this.open_maintenance_menu(position, window, cx);
                        })),
                    ),
            )
    }
}

impl Render for SchemaTree {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Measure the user's actual font once per frame, at the two sizes the
        // rows use, and size every character-counted cell from that. Shaping per
        // row would be hundreds of calls a frame; the advance is the same for
        // all of them.
        let name_advance = Theme::char_advance(window, Theme::font_size());
        let detail_advance = Theme::char_advance(window, Theme::font_size_ratio(0.74));
        // The rail's current inner width, used to decide how much column
        // metadata fits before it would collide with the names. Zero on the
        // first frame (before layout has run) — treated as "show everything"
        // so a freshly opened rail never blinks its metadata off.
        let rail_width = f32::from(self.scroll.bounds().size.width);

        let compiled = Rc::clone(&self.compiled);
        let visible = self.cached_flatten();

        if visible.is_empty() {
            // Two different nothings. "No connections yet" is a first-run state;
            // a filter that matches nothing is a state the user typed their way
            // into and can type their way out of, and saying which is which is
            // the difference between a dead end and a next step.
            let first_run = self.roots.is_empty();
            let (headline, hint) = if first_run {
                (
                    crate::localization::text("explorer.schema_render.empty_connections_title"),
                    crate::localization::text("explorer.schema_render.empty_connections_hint"),
                )
            } else {
                (
                    crate::localization::format(
                        "explorer.schema_render.no_matches_title",
                        &[("filter", &self.filter.display_text())],
                    ),
                    crate::localization::text("explorer.schema_render.no_matches_hint"),
                )
            };
            return div()
                .flex()
                .flex_col()
                .flex_1()
                .items_center()
                .justify_center()
                .gap(px(Theme::scaled(10.0)))
                .p(px(Theme::scaled(24.0)))
                .child(
                    Icon::empty()
                        .path(if first_run {
                            crate::icon_catalog::AppIcon::Globe.path()
                        } else {
                            crate::icon_catalog::AppIcon::Search.path()
                        })
                        .size(px(Theme::scaled(22.0)))
                        .text_color(Theme::text_dim().opacity(0.6)),
                )
                .child(
                    div()
                        .text_size(px(Theme::font_size()))
                        .text_color(Theme::text())
                        .child(headline),
                )
                .child(
                    div()
                        .max_w(px(Theme::scaled(220.0)))
                        .whitespace_normal()
                        .text_align(gpui::TextAlign::Center)
                        .text_size(px(Theme::font_size_ratio(0.85)))
                        .text_color(Theme::text_dim())
                        .child(hint),
                )
                .into_any_element();
        }

        let context_menu = self.context_menu.clone();
        let selected = self.selected.clone();

        let scrollbar_w = Theme::scaled(15.0);
        let viewport_h = f32::from(self.scroll.bounds().size.height);
        let max_scroll = f32::from(self.scroll.max_offset().y);
        let offset_y = f32::from(self.scroll.offset().y);
        let (scrollable, thumb_h, thumb_y) = if max_scroll > 0.0 && viewport_h > 0.0 {
            let content_total = max_scroll + viewport_h;
            // The thumb never shrinks below a grabbable size — scaled, like
            // every other measurement, since it is a pointer target.
            let thumb_h = ((viewport_h * viewport_h) / content_total).max(Theme::scaled(24.0));
            let pct = (-offset_y / max_scroll).clamp(0.0, 1.0);
            let thumb_y = pct * (viewport_h - thumb_h);
            (true, thumb_h, thumb_y)
        } else {
            (false, 0.0, 0.0)
        };

        let row_h = Theme::row_height();
        let (win_start, win_end) = viewport_window(visible.len(), viewport_h, offset_y, row_h);

        let scroll_content = div()
            .id("schema_tree_scroll")
            .flex()
            .flex_col()
            .flex_1()
            // Without min-height:0 a flex child defaults to min-height:auto and
            // grows to its content size, so overflow_scroll never engages. This
            // lets the container shrink to its parent and scroll instead.
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.scroll)
            .when(scrollable, |d| d.pr(px(scrollbar_w)))
            .child(
                div()
                    .flex()
                    .flex_col()
                    // Rows fill the rail and ellipsize; nothing extends past the
                    // right edge, so there is no horizontal scroll range to
                    // reserve a track for. Reading a long name is the tooltip's
                    // and the filter's job, not a sideways drag's.
                    .w_full()
                    .when(win_start > 0, |d| {
                        d.child(div().h(px(row_h * win_start as f32)))
                    })
                    .children(visible[win_start..win_end].iter().cloned().map(|flat| {
                        let FlatNode {
                            id: node_id,
                            depth,
                            title,
                            title_lower,
                            detail,
                            kind,
                            expanded,
                            load_state,
                            child_count,
                            align,
                            env_color_hex,
                            row_group,
                            tier: _,
                        } = flat;
                        // Alignment only applies when the matching Data Model
                        // preference is on for this surface.
                        let align =
                            align.filter(|a| self.data_model_prefs.align_enabled(a.surface));
                        // The row's whole left offset (gutter + one step per
                        // level), scaled like every other measurement.
                        let indent = px(Theme::scaled(
                            TREE_ROW_INSET + depth as f32 * TREE_LEVEL_INDENT,
                        ));
                        let is_expandable = kind.is_expandable();

                        let server_id: Option<Uuid> = match &kind {
                            NodeKind::Server { id } => Some(*id),
                            _ => None,
                        };
                        // Health is the node's own observed state — never a
                        // statement about which server the focused query
                        // window happens to be bound to.
                        let conn_state = server_id
                            .map(|sid| self.connection_state(sid))
                            .unwrap_or_default();
                        let is_connected_server =
                            conn_state == crate::schema_tree::ConnectionState::Connected;
                        // While a multi-selection is active the fill means
                        // "in the set" and the keyboard cursor wears a ring
                        // instead, so toggling the cursor row out (Space or
                        // Cmd/Ctrl-click) visibly un-fills it.
                        let is_cursor = selected.as_deref() == Some(node_id.as_ref());
                        let is_selected = if self.multi.is_empty() {
                            is_cursor
                        } else {
                            self.multi.contains(node_id.as_ref())
                        };
                        let cursor_ring = is_cursor && !self.multi.is_empty();
                        // One group per row so the edit/del links can reveal on
                        // hover of the whole row rather than of themselves.

                        // A grouping header paints its entity's identity: the
                        // color as a contrast-clamped background tint, the icon
                        // in the gutter the Azure account icon already uses.
                        let (group_tint, group_image) = match &kind {
                            NodeKind::Group {
                                color_hex,
                                image_path,
                            } => (
                                color_hex.as_deref().and_then(crate::theme::parse_hex_color),
                                image_path.clone().map(std::path::PathBuf::from),
                            ),
                            _ => (None, None),
                        };

                        // A server row is a name and nothing else — the host it
                        // points at and the credentials it uses live only in the
                        // status bar, and only for the active one. Two profiles
                        // called "Reporting" are otherwise identical on screen.
                        // Whatever the base, the failure reason gets appended.
                        let base_tooltip = match server_id
                            .and_then(|sid| self.profiles.iter().find(|p| p.id.0 == sid))
                        {
                            Some(p) => server_tooltip(p, conn_state),
                            None => object_tooltip(&kind, &title, detail.as_deref(), &node_id),
                        };
                        let row_tooltip = with_load_state(base_tooltip, &load_state);

                        let row_base = div()
                            .id(gpui::SharedString::from(format!("schema-row-{node_id}")))
                            .group(row_group.clone())
                            .flex()
                            .flex_row()
                            .items_center()
                            .flex_shrink_0()
                            // Anchor for the server row's hover affordances,
                            // which float above the row instead of occupying it.
                            .relative()
                            .w_full()
                            // Deliberately NOT `whitespace_nowrap` here. Text
                            // style cascades, and gpui only draws a truncation
                            // ellipsis when it recomputes the layout at a known
                            // width — which it skips for nowrap text, returning
                            // the cached full-width measurement instead
                            // (`gpui::elements::text`). An inherited nowrap
                            // therefore clipped names to "prod-reporti" with no
                            // "…" to say they were cut. Each cell that needs it
                            // asks for nowrap itself; the name asks for
                            // `truncate`, and the fixed row height plus
                            // `overflow_hidden` keep it to one line regardless.
                            .gap(px(Theme::scaled(4.0)))
                            // Indent is padding, not a margin: a `w_full` row
                            // with a left margin would push its own right edge
                            // out of the rail and reintroduce overflow.
                            .pl(indent)
                            .pr(px(Theme::scaled(2.0)))
                            .h(px(Theme::row_height()))
                            // macOS source lists use the arrow cursor; the
                            // pointing hand is a web affordance.
                            .cursor(gpui::CursorStyle::Arrow)
                            .text_size(px(Theme::font_size_ratio(0.85)))
                            .when(cursor_ring, |d| {
                                d.border_1().border_color(Theme::accent().opacity(0.7))
                            });

                        // Every foreground in this row, resolved against its
                        // selected state. On a selected row the quiet fill
                        // carries full ink and nothing else (see
                        // `themed_sidebar_row`), so muted ink, kind glyphs and
                        // status colors all lift rather than drown.
                        let fg_dim = Theme::selection_fg(is_selected, Theme::text_dim());
                        let fg_kind = Theme::selection_fg(is_selected, kind_color(&kind));
                        let fg_error = Theme::selection_fg(is_selected, Theme::error());

                        // What this row's background becomes on hover. The
                        // floating edit/del strip paints itself with the same
                        // fill so it reads as part of the row it covers.
                        let hover_bg = match group_tint {
                            Some(entity) => Theme::entity_tint_over(entity, Theme::row_hover()),
                            None => Theme::row_hover(),
                        };

                        // Exactly one `hover` per row — gpui panics in debug if
                        // an element sets it twice.
                        let row = match group_tint {
                            // A tinted group header keeps its identity on hover
                            // by re-tinting the hover surface rather than being
                            // repainted flat gray. Selection still wins over the
                            // tint, though: an identity color is standing
                            // information, and the selection is the answer to
                            // "where am I right now".
                            Some(entity) if !is_selected => row_base
                                .bg(Theme::entity_tint(entity))
                                .hover(move |s| s.bg(hover_bg)),
                            // Selected is a quiet lift, not an amber block.
                            // Connected is the icon's job, not the fill's.
                            _ => themed_sidebar_row(row_base, is_selected),
                        };
                        // A saved server wears its environment's identity color
                        // as a small dot between the marker and the name, so
                        // "which environment is this?" is answered in the rail
                        // itself, not only in the status bar — and there only
                        // for the connected server. The raw hex lives on the
                        // node; the palette resolution happens here, per frame.
                        // The dot keeps its color on a selected row, the same
                        // standing-information treatment the group tint gets.
                        let env_dot = env_color_hex
                            .as_deref()
                            .and_then(crate::theme::parse_hex_color)
                            .map(|color| {
                                div()
                                    .flex_shrink_0()
                                    .w(px(Theme::scaled(6.0)))
                                    .h(px(Theme::scaled(6.0)))
                                    .rounded_full()
                                    .bg(color)
                                    .into_any_element()
                            });

                        let health_dot = server_health(&kind, &load_state, conn_state)
                            .filter(|health| {
                                // Failed loads paint their own clickable
                                // status dot (the retry control). Don't
                                // stack a second red circle beside it.
                                !matches!(
                                    (health, &load_state),
                                    (ServerHealth::Failed, LoadState::Failed(_))
                                )
                            })
                            .map(|health| {
                                let (color, pulse) = match health {
                                    ServerHealth::Connected => (Theme::success(), false),
                                    ServerHealth::Connecting => (Theme::warning(), true),
                                    ServerHealth::Failed => (Theme::error(), false),
                                    // Last-known state from a previous session:
                                    // the same hue, but visibly aged — never a
                                    // claim of live connectivity.
                                    ServerHealth::StaleConnected => {
                                        (Theme::success().opacity(0.4), false)
                                    }
                                    ServerHealth::StaleFailed => {
                                        (Theme::error().opacity(0.4), false)
                                    }
                                };
                                let color = Theme::selection_fg(is_selected, color);
                                let dot = div()
                                    .flex_shrink_0()
                                    .w(px(Theme::scaled(6.0)))
                                    .h(px(Theme::scaled(6.0)))
                                    .rounded_full()
                                    .bg(color);
                                if pulse {
                                    dot.with_animation(
                                        ElementId::Name(format!("srv-health-{node_id}").into()),
                                        Animation::new(Duration::from_millis(1100)).repeat(),
                                        |this, delta| {
                                            let phase =
                                                (delta * std::f32::consts::TAU).sin() * 0.5 + 0.5;
                                            this.opacity(0.35 + 0.65 * phase)
                                        },
                                    )
                                    .into_any_element()
                                } else {
                                    dot.into_any_element()
                                }
                            });

                        let row = row
                            .children(health_dot)
                            .when(is_expandable, |row| {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .w(px(Theme::scaled(12.0)))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .text_color(fg_dim)
                                        .child(crate::icons::disclosure_chevron(expanded)),
                                )
                            })
                            .children({
                                // The gutter only reserves a slot for kinds that
                                // paint something real: the Azure account image, a
                                // group's entity image, or an object-explorer SVG
                                // glyph (server, database, table, view, procedure,
                                // function, column, index). Kinds that used to fall
                                // through to a "T"/"V"/"P"/"SP"/"FN" letter — or a
                                // blank placeholder — now contribute no gutter child
                                // at all, so the label slides left into that space.
                                // Disclosure arrows and indentation still carry the
                                // hierarchy.
                                let gutter: Option<gpui::AnyElement> =
                                    if matches!(kind, NodeKind::AzureAccount) {
                                        Some(
                                            crate::icons::entra_id_icon()
                                                .map(|icon| {
                                                    img(icon)
                                                        .flex_shrink_0()
                                                        .w(px(Theme::font_size() * 1.4))
                                                        .h(px(Theme::font_size() * 1.1))
                                                        .into_any_element()
                                                })
                                                .unwrap_or_else(|| {
                                                    // The only render-path use
                                                    // of the legacy text marker
                                                    // — computed here, not per
                                                    // row.
                                                    div()
                                                        .flex_shrink_0()
                                                        .w(px(Theme::font_size() * 1.4))
                                                        .text_size(px(Theme::font_size_ratio(0.67)))
                                                        .text_color(fg_kind)
                                                        .child(kind.icon())
                                                        .into_any_element()
                                                }),
                                        )
                                    } else if let Some(image) = group_image.clone() {
                                        // A group header carrying an entity image
                                        // paints it in the same gutter the Azure
                                        // icon uses.
                                        Some(
                                            img(image)
                                                .flex_shrink_0()
                                                .w(px(Theme::font_size() * 1.4))
                                                .h(px(Theme::font_size() * 1.1))
                                                .into_any_element(),
                                        )
                                    } else {
                                        // Object-explorer entity kinds draw a real
                                        // SVG glyph. Its ink follows the gutter's
                                        // text color, so the active-server green
                                        // still applies. Folders and schemas have
                                        // no glyph and so contribute no gutter
                                        // child, letting the label slide left.
                                        kind.glyph_icon().map(|glyph| {
                                            div()
                                                .flex_shrink_0()
                                                .w(px(Theme::font_size() * 1.4))
                                                .text_color(if is_connected_server {
                                                    Theme::selection_fg(
                                                        is_selected,
                                                        Theme::success(),
                                                    )
                                                } else {
                                                    fg_kind
                                                })
                                                .child(
                                                    Icon::empty()
                                                        .path(glyph)
                                                        .size(px(Theme::font_size() * 1.1)),
                                                )
                                                .into_any_element()
                                        })
                                    };
                                gutter
                            })
                            .children(env_dot)
                            .child(
                                div()
                                    .id(gpui::SharedString::from(format!("schema-name-{node_id}")))
                                    .text_color(Theme::text())
                                    // A group header reads as one from its
                                    // entity tint and position in the rail, not
                                    // a heavier weight — the One Instrument Rule
                                    // keeps the whole app at a single weight and
                                    // spends hierarchy on surface and space.
                                    // The name is what yields space: it
                                    // ellipsizes so the row always fits the rail,
                                    // and the tooltip carries the full text.
                                    // `min_w_0` is what actually lets a flex item
                                    // shrink below its content width.
                                    //
                                    // It has no minimum width. It used to keep an
                                    // 8-character floor, which is a fine idea and
                                    // the wrong mechanism: a floor the rail can't
                                    // honor doesn't reserve space, it *overflows*
                                    // — at an enlarged interface font the name
                                    // stopped shrinking, the ellipsis never
                                    // engaged, and the text was clipped mid-glyph
                                    // at the pane edge. Enlarging the interface
                                    // font is the accessibility lever this app
                                    // has; it must not be the one that breaks the
                                    // layout. The rail is resizable and the
                                    // tooltip holds the full name, so yielding is
                                    // the right failure.
                                    .min_w_0()
                                    .overflow_hidden()
                                    .text_ellipsis()
                                    // Aligned metadata rows: pin the name cell
                                    // to the widest sibling so the data-type
                                    // column starts on one vertical line. It can
                                    // still shrink when the rail is too narrow to
                                    // honor the alignment.
                                    .map(|d| match align {
                                        Some(a) => {
                                            d.w(px(Theme::cell_width(a.name_chars, name_advance)))
                                        }
                                        None => d.flex_1(),
                                    })
                                    // Wrapping, not the plain chip: identifiers
                                    // run to 128 characters each and a qualified
                                    // name is three of them, so an uncapped
                                    // tooltip is wider than the display.
                                    .app_tooltip_wrapping(row_tooltip.clone())
                                    .child(highlighted_title(
                                        &title,
                                        title_lower.as_ref(),
                                        compiled.as_ref(),
                                    )),
                            );

                        let row = if let Some(d) = &detail {
                            if let Some(a) = align {
                                // "{type}, {attrs}" → fixed-width type cell +
                                // attrs cell, so nullability lines up too.
                                // Widths/alignment honor the Metadata Columns
                                // preferences; width 0 = auto (widest sibling).
                                let (dtype, attrs) = match d.split_once(", ") {
                                    Some((t, rest)) => (t.to_string(), rest.to_string()),
                                    None => (d.to_string(), String::new()),
                                };
                                let dt_fmt = self.data_model_prefs.data_type_format;
                                let null_fmt = self.data_model_prefs.nullability_format;
                                let type_chars = if dt_fmt.width_chars > 0 {
                                    dt_fmt.width_chars
                                } else {
                                    a.type_chars
                                };
                                // Identity is the first thing to shed: it lives
                                // as a trailing ", identity" on the attrs cell,
                                // built in the metadata service. Dropping it
                                // leaves the nullability behind.
                                let attrs_noident = attrs
                                    .strip_suffix(", identity")
                                    .map(str::to_string)
                                    .unwrap_or_else(|| attrs.clone());
                                // Decide, from the rail's real width, how much
                                // of {name} {type} {attrs} actually fits. The
                                // name column is fixed (aligned), so everything
                                // to its right is what has to give.
                                let level = if rail_width <= 0.0 {
                                    MetaLevel::All
                                } else {
                                    let gap = Theme::scaled(4.0);
                                    let chevron = Theme::scaled(12.0);
                                    let glyph = Theme::font_size() * 1.4;
                                    // Row content box: rail minus the scrollbar
                                    // gutter, minus this row's own indent and
                                    // right padding.
                                    let content = rail_width
                                        - scrollbar_w
                                        - f32::from(indent)
                                        - Theme::scaled(2.0);
                                    // Disclosure glyph, kind icon, the two gaps
                                    // before the name, and the aligned name
                                    // column — the space the metadata starts
                                    // after.
                                    let name_w = Theme::cell_width(a.name_chars, name_advance);
                                    let after_name =
                                        content - (chevron + gap + glyph + gap + name_w);
                                    let type_need = gap
                                        + Theme::scaled(8.0)
                                        + Theme::cell_width(type_chars, detail_advance);
                                    let attrs_need = |s: &str| {
                                        gap + if null_fmt.width_chars > 0 {
                                            Theme::cell_width(null_fmt.width_chars, detail_advance)
                                        } else {
                                            Theme::cell_width(s.chars().count(), detail_advance)
                                        }
                                    };
                                    if after_name < 0.0 {
                                        MetaLevel::None
                                    } else if after_name >= type_need + attrs_need(&attrs) {
                                        MetaLevel::All
                                    } else if after_name >= type_need + attrs_need(&attrs_noident) {
                                        MetaLevel::NoIdentity
                                    } else if after_name >= type_need {
                                        MetaLevel::TypeOnly
                                    } else {
                                        MetaLevel::None
                                    }
                                };
                                let align_cell =
                                    |cell: gpui::Div,
                                     alignment: crate::preferences::MetadataAlignment|
                                     -> gpui::Div {
                                        match alignment {
                                            crate::preferences::MetadataAlignment::Left => {
                                                cell.justify_start()
                                            }
                                            crate::preferences::MetadataAlignment::Right => {
                                                cell.justify_end()
                                            }
                                        }
                                    };
                                // A fixed-width cell clips rather than spills.
                                // `width_chars` is a user preference, so someone
                                // can pin the data-type column to 4 characters
                                // while `nvarchar(4000)` needs 14 — and the
                                // character-count estimate is wrong anyway for
                                // any name that isn't single-width Latin: CJK
                                // renders double-width, emoji wider still. Both
                                // end with text sitting on top of its neighbour
                                // unless the cell is told to keep its content.
                                // The data type shows for every level except
                                // `None`; identity/nullability follow the level.
                                if level == MetaLevel::None {
                                    row
                                } else {
                                    let row = row.child(
                                        align_cell(div().flex().flex_row(), dt_fmt.alignment)
                                            .ml(px(Theme::scaled(8.0)))
                                            .flex_shrink_0()
                                            .w(px(Theme::cell_width(type_chars, detail_advance)))
                                            .overflow_hidden()
                                            .whitespace_nowrap()
                                            .text_size(px(Theme::font_size_ratio(0.74)))
                                            .text_color(fg_dim)
                                            .child(dtype),
                                    );
                                    let attrs = match level {
                                        MetaLevel::All => Some(attrs),
                                        MetaLevel::NoIdentity => Some(attrs_noident),
                                        // TypeOnly drops nullability too; None
                                        // never reaches here.
                                        _ => None,
                                    };
                                    match attrs {
                                        None => row,
                                        Some(attrs) => row.child({
                                            let cell = align_cell(
                                                div().flex().flex_row(),
                                                null_fmt.alignment,
                                            )
                                            .flex_shrink_0()
                                            .whitespace_nowrap()
                                            .text_size(px(Theme::font_size_ratio(0.74)))
                                            .text_color(fg_dim);
                                            // Auto width keeps the old
                                            // content-sized behavior; a set width
                                            // makes a real column so
                                            // right-alignment has room to act.
                                            if null_fmt.width_chars > 0 {
                                                cell.w(px(Theme::cell_width(
                                                    null_fmt.width_chars,
                                                    detail_advance,
                                                )))
                                                .overflow_hidden()
                                                .child(localized_column_attributes(&attrs))
                                            } else {
                                                cell.child(localized_column_attributes(&attrs))
                                            }
                                        }),
                                    }
                                }
                            } else {
                                row.child(
                                    div()
                                        .ml(px(Theme::scaled(8.0)))
                                        .text_size(px(Theme::font_size_ratio(0.74)))
                                        .text_color(fg_dim)
                                        .child(localized_column_attributes(d)),
                                )
                            }
                        } else {
                            row
                        };

                        // Load state reads as a short, fixed-width marker. The
                        // full text lives in the row's tooltip: painting a raw
                        // TDS error into the row stretched the tree's scroll
                        // width to the length of the message.
                        let marker = |label: &'static str, color: gpui::Hsla| {
                            div()
                                .flex_shrink_0()
                                .ml(px(Theme::scaled(8.0)))
                                .text_size(px(Theme::font_size_ratio(0.74)))
                                .text_color(color)
                                .child(crate::localization::text(label))
                        };
                        // A folder that has loaded says how much it found.
                        // "(0)" would read as a glitch, so nothing found says so
                        // in words.
                        let row =
                            if matches!(load_state, LoadState::Loaded) && is_folder_kind(&kind) {
                                row.child(
                                    div()
                                        .flex_shrink_0()
                                        .ml(px(Theme::scaled(8.0)))
                                        // Keep the count off the rail's right
                                        // edge; without this it sits almost
                                        // flush against the scroll boundary.
                                        .mr(px(Theme::scaled(4.0)))
                                        .text_size(px(Theme::font_size_ratio(0.74)))
                                        .text_color(fg_dim)
                                        .child(if child_count == 0 {
                                            crate::localization::text(
                                                "explorer.schema_render.folder_empty",
                                            )
                                        } else {
                                            format!("{child_count}")
                                        }),
                                )
                            } else {
                                row
                            };

                        let row = match &load_state {
                            LoadState::Loading => {
                                row.child(marker("explorer.schema_render.loading_marker", fg_dim))
                            }
                            // Status dot is the marker. Click it to retry; the
                            // tooltip + Messages carry the reason. Never paint
                            // the word "failed" on the row.
                            LoadState::Failed(_) => row.child(
                                div()
                                    .id(gpui::SharedString::from(format!("schema-retry-{node_id}")))
                                    .flex_shrink_0()
                                    .ml(px(Theme::scaled(8.0)))
                                    .w(px(Theme::scaled(6.0)))
                                    .h(px(Theme::scaled(6.0)))
                                    .rounded_full()
                                    .bg(fg_error)
                                    .cursor(gpui::CursorStyle::PointingHand)
                                    .app_tooltip_wrapping(crate::localization::format(
                                        "explorer.schema_render.retry_tooltip",
                                        &[("tooltip", &row_tooltip)],
                                    ))
                                    .children(failed_load_inline_word().map(|word| {
                                        div()
                                            .text_size(px(Theme::font_size_ratio(0.74)))
                                            .text_color(fg_error)
                                            .child(word)
                                    }))
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener({
                                            let id = node_id.clone();
                                            move |this, _, _, cx| {
                                                cx.stop_propagation();
                                                this.refresh_node(&id, cx);
                                            }
                                        }),
                                    ),
                            ),
                            // Not an error: a locked-down database is doing
                            // exactly what it was configured to do. It reads as
                            // muted state, not as red.
                            LoadState::PermissionDenied(_) => {
                                row.child(marker("explorer.schema_render.no_access_marker", fg_dim))
                            }
                            _ => row,
                        };

                        let row = if server_id.is_some() {
                            let kind_for_more = kind.clone();
                            let node_id_for_more = node_id.to_string();
                            let title_for_more = title.to_string();
                            row.child(
                                div()
                                    .id(gpui::SharedString::from(format!(
                                        "schema-srv-more-{node_id}"
                                    )))
                                    .flex_shrink_0()
                                    .ml_auto()
                                    .p(px(Theme::scaled(4.0)))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    // Visible on hover or selection so keyboard
                                    // users (selected row, no pointer) can still
                                    // reach connection actions.
                                    .when(!is_selected, |d| {
                                        d.invisible()
                                            .group_hover(row_group.clone(), |s| s.visible())
                                    })
                                    .text_color(fg_dim)
                                    .cursor(gpui::CursorStyle::PointingHand)
                                    .hover(|s| s.text_color(Theme::text()))
                                    .app_tooltip(|_window, cx| {
                                        crate::app::tooltip_view(
                                            crate::localization::text(
                                                "explorer.schema_render.connection_actions_tooltip",
                                            ),
                                            cx,
                                        )
                                    })
                                    .child(
                                        Icon::empty()
                                            .path(crate::icon_catalog::AppIcon::MoreVertical.path())
                                            .size(px(Theme::toolbar_icon_size())),
                                    )
                                    .on_mouse_down(
                                        gpui::MouseButton::Left,
                                        cx.listener(
                                            move |this, ev: &MouseDownEvent, window, cx| {
                                                cx.stop_propagation();
                                                this.open_context_menu(
                                                    node_id_for_more.clone(),
                                                    kind_for_more.clone(),
                                                    title_for_more.clone(),
                                                    ev.position,
                                                    window,
                                                    cx,
                                                );
                                            },
                                        ),
                                    ),
                            )
                        } else {
                            row
                        };

                        // Left click: server rows connect + expand; table/view
                        // rows keep double-click-to-open; everything else toggles.
                        let kind_for_click = kind.clone();
                        let node_id_for_click = node_id.to_string();
                        let row = row.on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(
                                move |this, ev: &MouseDownEvent, window: &mut Window, cx| {
                                    // Clicking the tree gives it the keyboard, so
                                    // arrows continue from what was just clicked.
                                    window.focus(&this.focus_handle, cx);
                                    // Ctrl/Cmd toggles and shift ranges are selection
                                    // gestures, not row behavior: consumed here, the
                                    // row neither expands nor opens.
                                    if this.handle_multi_click(
                                        &node_id_for_click,
                                        &kind_for_click,
                                        &ev.modifiers,
                                        cx,
                                    ) {
                                        return;
                                    }
                                    let now = Instant::now();
                                    let is_double = this.last_click_id.as_deref()
                                        == Some(&node_id_for_click)
                                        && this
                                            .last_click_time
                                            .map(|t| now.duration_since(t).as_millis() < 500)
                                            .unwrap_or(false);
                                    this.last_click_id = Some(node_id_for_click.clone());
                                    this.last_click_time = Some(now);
                                    // Clicking anything selects it. Group headers
                                    // are structure, not objects, so they don't take
                                    // the mark.
                                    if !matches!(kind_for_click, NodeKind::Group { .. }) {
                                        this.selected = Some(node_id_for_click.clone());
                                    }

                                    if matches!(kind_for_click, NodeKind::Server { .. }) {
                                        // Browsing is not connecting. Expanding a
                                        // server loads its databases over that
                                        // server's own credentials and touches
                                        // nothing else — the query tab keeps the
                                        // connection it is running against.
                                        //
                                        // It used to retarget the whole app here, so
                                        // opening one server to look up a column name
                                        // silently moved the editor, the completion
                                        // catalog and the next execution onto it.
                                        // Connecting is now the deliberate gesture it
                                        // reads as: Enter on the row, or Connect from
                                        // the context menu.
                                        this.toggle_expand(&node_id_for_click, cx);
                                        return;
                                    }

                                    if let NodeKind::AzureDatabase(discovery) = &kind_for_click {
                                        cx.emit(SchemaTreeEvent::LinkAzureDatabase(
                                            (**discovery).clone(),
                                        ));
                                        return;
                                    }

                                    if let NodeKind::Pinned { index } = &kind_for_click {
                                        if let Some(server_id) =
                                            server_uuid_from_id(&node_id_for_click)
                                        {
                                            this.open_favorite(server_id, *index, cx);
                                        }
                                        return;
                                    }

                                    // Double-clicking a Redis key opens the key
                                    // view (header, paged value, edits).
                                    if is_double
                                        && let NodeKind::RedisKey { key, kind, .. } =
                                            &kind_for_click
                                    {
                                        cx.emit(SchemaTreeEvent::OpenRedisKey {
                                            key: key.clone(),
                                            kind: kind.clone(),
                                            server_id: server_uuid_from_id(&node_id_for_click),
                                            database: Some(extract_db_from_id(&node_id_for_click))
                                                .filter(|d| !d.is_empty()),
                                        });
                                        return;
                                    }

                                    // Double-clicking a search index opens its
                                    // definition and statistics (FT.INFO).
                                    if is_double
                                        && let NodeKind::RedisIndex { name } = &kind_for_click
                                    {
                                        cx.emit(SchemaTreeEvent::RunCommandInNewTab {
                                            sql: crate::schema_tree::redis::index_info_command(
                                                name,
                                            ),
                                            title: None,
                                            server_id: server_uuid_from_id(&node_id_for_click),
                                            database: Some(extract_db_from_id(&node_id_for_click))
                                                .filter(|d| !d.is_empty()),
                                        });
                                        return;
                                    }

                                    // The key browser's note row is its own
                                    // control: a click shows more of the listing
                                    // (the next window, or the continued scan).
                                    if matches!(kind_for_click, NodeKind::RedisNote { .. }) {
                                        this.redis_note_activated(&node_id_for_click, cx);
                                        return;
                                    }

                                    if is_double
                                        && let NodeKind::Table { schema, name }
                                        | NodeKind::View { schema, name } = &kind_for_click
                                    {
                                        cx.emit(SchemaTreeEvent::SelectObject {
                                            schema: schema.clone(),
                                            name: name.clone(),
                                            server_id: server_uuid_from_id(&node_id_for_click),
                                            database: Some(extract_db_from_id(&node_id_for_click))
                                                .filter(|d| !d.is_empty()),
                                        });
                                        return;
                                    }

                                    if kind_for_click.is_expandable() {
                                        this.toggle_expand(&node_id_for_click, cx);
                                    }
                                },
                            ),
                        );

                        // Right click: context menu for all node kinds except
                        // Group headers (which have no meaningful menu).
                        let row = if !matches!(kind, NodeKind::Group { .. }) {
                            let kind_for_right = kind.clone();
                            let node_id_for_right = node_id.to_string();
                            let title_for_right = title.to_string();
                            row.on_mouse_down(
                                gpui::MouseButton::Right,
                                cx.listener(move |this, ev: &MouseDownEvent, window, cx| {
                                    this.open_context_menu(
                                        node_id_for_right.clone(),
                                        kind_for_right.clone(),
                                        title_for_right.clone(),
                                        ev.position,
                                        window,
                                        cx,
                                    );
                                }),
                            )
                        } else {
                            row
                        };

                        if let Some(dragged) =
                            dragged_identifier_for(&self.roots, node_id.as_ref(), &kind)
                        {
                            row.on_drag(dragged, |value, _offset, _window, cx| {
                                cx.new(|_| DragPreview {
                                    label: value.object_display(),
                                })
                            })
                        } else {
                            row
                        }
                    }))
                    .when(win_end < visible.len(), |d| {
                        d.child(div().h(px(row_h * (visible.len() - win_end) as f32)))
                    }),
            );

        let tree = div()
            .id("schema-tree")
            // Arrow keys walk and expand, Enter opens. Without this the rail
            // was reachable only by mouse.
            .key_context("SchemaTree")
            .track_focus(&self.focus_handle)
            // A tab stop, or none of the above is reachable. `track_focus` only
            // makes the tree focusable *programmatically* — which meant the only
            // way in was a mouse click on a row, so the keyboard navigation was
            // itself mouse-gated and did nothing for the person it was for.
            .tab_index(0)
            .tab_stop(true)
            .on_key_down(cx.listener(Self::on_key_down))
            .flex()
            .flex_col()
            .flex_1()
            .w_full()
            .min_h_0()
            .relative()
            // The multi-selection banner pins above the rows while the set is
            // non-empty; it never scrolls away with them.
            .when(!self.multi.is_empty(), |d| {
                d.child(self.render_bulk_banner(cx))
            })
            .child(scroll_content)
            // Track and thumb both appear only when there is something to
            // scroll. A track painted over an unscrollable list is 15px of a
            // ~250px rail spent saying nothing. The horizontal pair is gone
            // entirely: rows ellipsize now, so nothing ever extends past the
            // right edge.
            .when(scrollable, |d| {
                d.child(
                    // The track is the hit target: pressing anywhere on it jumps
                    // there, and holding drags. The thumb below is paint only,
                    // sitting inside this.
                    div()
                        .id("schema-scrollbar")
                        .absolute()
                        .top_0()
                        .right_0()
                        .w(px(scrollbar_w))
                        .h_full()
                        .bg(Theme::border().opacity(0.3))
                        .cursor(gpui::CursorStyle::Arrow)
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|this, ev: &MouseDownEvent, _, cx| {
                                this.scrollbar_dragging = true;
                                this.scroll_to_pointer(ev.position.y);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .absolute()
                                .top(px(thumb_y))
                                .left_0()
                                .w(px(scrollbar_w))
                                .h(px(thumb_h))
                                .bg(Theme::text_dim().opacity(0.5)),
                        ),
                )
            })
            // Drag tracking lives on the container so the pointer can wander off
            // the 15px track mid-drag without the thumb stopping dead.
            .on_mouse_move(cx.listener(|this, ev: &gpui::MouseMoveEvent, _, cx| {
                if this.scrollbar_dragging {
                    this.scroll_to_pointer(ev.position.y);
                    cx.notify();
                }
            }))
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    if this.scrollbar_dragging {
                        this.scrollbar_dragging = false;
                        cx.notify();
                    }
                }),
            );

        if let (Some(menu), Some(menu_view)) = (context_menu, self.context_menu_view.clone()) {
            // `menu.position` is `ev.position` from the right-click, in window
            // coordinates. Position the menu with `anchored()`, which resolves
            // against the window (not the local containing block) and auto-flips
            // at viewport edges. `deferred()` paints it above the tree, and
            // `.occlude()` + `.on_mouse_down_out()` dismiss it on an outside
            // click. A plain `.absolute()` child would be offset by the tree
            // panel's position within the window, making the menu appear below
            // the click point.
            // NB: no `.on_mouse_down_out` handler here. A submenu is a separate
            // `deferred` popover drawn to the *right* of this wrapper, so
            // clicking a submenu item is a mouse-down *outside* this div. An
            // outside-click handler here fires on that click and dismisses the
            // whole menu before the submenu item's own `on_click` runs — which
            // silently swallowed every Tools-submenu launch. `PopupMenu` already
            // handles outside clicks itself (its handler skips dismissal while a
            // submenu is open) and emits `DismissEvent`, which the tree
            // subscribes to (`schema_tree`), so dismissal still works without a
            // second, submenu-unaware handler here.
            let overlay = deferred(
                anchored()
                    .position(point(menu.position.x, menu.position.y))
                    .child(
                        div()
                            .occlude()
                            // The popup paints gpui-component's own shadcn
                            // `shadow-md`, which barely separates a menu from a
                            // dense tree of rows behind it. This wrapper hugs
                            // the popup and paints a deeper, wider drop shadow
                            // underneath it; the popup's own shadow still draws
                            // on top, so the near-edge stays crisp.
                            .rounded(px(Theme::scaled(4.0)))
                            .shadow(context_menu_shadow())
                            .child(menu_view),
                    ),
            )
            .with_priority(1);

            div()
                .flex()
                .flex_col()
                .flex_1()
                .child(tree)
                .child(overlay)
                .into_any_element()
        } else {
            tree.into_any_element()
        }
    }
}

#[cfg(test)]
pub(crate) mod render_tests {
    use super::super::{
        build_hydrated_db_children, find_server_node, find_server_node_mut, merge_hydrated_children,
    };
    use super::*;
    use sqlly_core::AzureSqlDiscovery;
    use std::cell::RefCell;
    use std::rc::Rc;

    fn compile_q(needle: &str) -> crate::explorer_search::CompiledQuery {
        crate::explorer_search::CompiledQuery::compile(
            &crate::explorer_search::ExplorerQuery::parse(needle, Default::default()),
        )
    }

    /// gpui-component widgets read the library's global theme during render;
    /// windowed tests must install it (idempotently) before opening windows.
    pub(crate) fn ensure_components_initialized(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            if cx.try_global::<gpui_component::Theme>().is_none() {
                gpui_component::init(cx);
            }
        });
    }

    /// Wrapper that renders [`SchemaTree`] with a fixed offset from the
    /// window origin, so that panel-origin ≠ window-origin. This makes the
    /// old `.absolute()` positioning bug detectable: if the context menu is
    /// positioned relative to the panel rather than the window, it appears
    /// at `panel_origin + click_point` instead of `click_point`.
    pub(crate) struct OffsetTreeWrapper {
        tree: gpui::Entity<SchemaTree>,
        events: Rc<RefCell<Vec<SchemaTreeEvent>>>,
    }

    impl OffsetTreeWrapper {
        pub(crate) fn new(cx: &mut Context<Self>) -> Self {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let tree = cx.new(|cx| SchemaTree::new(services, log, cx));
            let events = Rc::new(RefCell::new(Vec::new()));
            let events_clone = events.clone();
            cx.subscribe(&tree, move |_this, _tree, event: &SchemaTreeEvent, _cx| {
                events_clone.borrow_mut().push(event.clone());
            })
            .detach();
            Self { tree, events }
        }
    }

    impl Render for OffsetTreeWrapper {
        fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .pt(px(50.0))
                .pl(px(50.0))
                .size_full()
                .child(self.tree.clone())
        }
    }

    /// Regression: right-clicking a tree node must open the context menu at
    /// the **window-space** click position, not at `panel_origin + click_point`.
    ///
    /// With the old `.absolute()` positioning, the menu was offset by the
    /// panel's position within the window. This test offsets the tree by
    /// (50, 50) px from the window origin, right-clicks a Database row, then
    /// left-clicks where the menu item should be in window space. If the menu
    /// is mispositioned, the click misses and no event is emitted.
    #[gpui::test]
    pub(crate) fn context_menu_appears_at_window_click_position(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            let wrapper = OffsetTreeWrapper::new(cx);
            wrapper.tree.update(cx, |tree, cx| {
                tree.set_azure_discoveries(
                    vec![AzureSqlDiscovery {
                        account_id: "acct-1".into(),
                        username: "user@example.com".into(),
                        tenant_id: "tenant-1".into(),
                        subscription_id: "sub-1".into(),
                        subscription_name: Some("My Sub".into()),
                        resource_group: "rg-1".into(),
                        server_id: "server-1".into(),
                        server_name: "my-server".into(),
                        server_fqdn: "my-server.database.windows.net".into(),
                        location: "eastus".into(),
                        database_id: "db-1".into(),
                        database_name: "player-app".into(),
                        service_tier: Some("GeneralPurpose".into()),
                        status: Some("Online".into()),
                        linked_profile_id: None,
                    }],
                    cx,
                );
            });
            wrapper
        });
        cx.run_until_parked();

        // 4 rows at Theme::row_height() each, offset by the wrapper's pt(50).
        let row_h = f32::from(px(Theme::row_height()));
        let panel_top = 50.0;
        let click_x = px(110.0);
        let click_y = px(panel_top + row_h * 3.5);

        // Right-click the Database row → menu opens at (110, click_y) in window coords
        cx.simulate_mouse_down(
            point(click_x, click_y),
            gpui::MouseButton::Right,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();

        let menu_open = view.read_with(cx, |v, cx| v.tree.read(cx).context_menu.is_some());
        assert!(
            menu_open,
            "right-click on Database row should open context menu"
        );
        // While the menu is up, tooltips are suppressed app-wide — otherwise
        // the row's own tooltip timer, started by the move that preceded the
        // right-click, paints a chip on top of the menu.
        assert!(
            crate::tooltip::menus_open(),
            "an open context menu must suppress tooltips"
        );

        // First menu item ("Copy Name") center in window space:
        //   With fix, menu top-left = (110, click_y).
        //   First item center ≈ (110 + 90, click_y + 18) = (200, click_y + 18).
        let menu_item_x = px(200.0);
        let menu_item_y = px(f32::from(click_y) + 18.0);

        cx.simulate_mouse_down(
            point(menu_item_x, menu_item_y),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(menu_item_x, menu_item_y),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();

        let events = view.read_with(cx, |v, _cx| v.events.borrow().clone());
        let event_count = events.len();
        assert!(
            events.iter().any(|e| matches!(
                e,
                SchemaTreeEvent::ExplorerActionRequested {
                    action: ExplorerMenuAction::CopyName,
                    ..
                }
            )),
            "clicking the menu item at its window-space position should emit a CopyName event, \
             but {} event(s) were captured and none matched ExplorerActionRequested{{CopyName}}",
            event_count
        );
        // Confirming an item closes the menu, which must hand tooltips back.
        assert!(
            !crate::tooltip::menus_open(),
            "closing the menu must release the tooltip suppression it took"
        );
    }

    /// `refresh_server` must clear the server's cached children
    /// and reset its load state, even when the server is nested inside a
    /// collapsed group header. This is the "full schema refresh" that the
    /// query-bar refresh button and `RefreshObjectExplorer` action trigger.
    #[gpui::test]
    pub(crate) fn refresh_server_clears_subtree(cx: &mut gpui::TestAppContext) {
        let p = {
            use sqlly_core::{SecretProvider, SecretRef};
            ConnectionProfile::sql_login(
                "Prod",
                "host",
                "sa",
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "k".into(),
                },
            )
        };
        let id = p.id.0;
        let sid = format!("srv:{id}");

        ensure_components_initialized(cx);
        let (tree, cx) = cx.add_window_view(|_window, cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            let mut tree = SchemaTree::new(services, log, cx);
            tree.set_connections(vec![p.clone()], cx);

            if let Some(server) = find_server_node_mut(&mut tree.roots, &sid) {
                // Keep expanded = false so refresh clears children without
                // triggering an async load_children (which needs a Tokio runtime).
                server.expanded = false;
                server.load_state = LoadState::Loaded;
                server.children = vec![TreeNode::new(
                    format!("{sid}:db:master"),
                    "master",
                    NodeKind::Database("master".into()),
                    1,
                )];
            }
            tree
        });

        cx.run_until_parked();

        tree.read_with(cx, |t, _cx| {
            let server = find_server_node(&t.roots, &sid).unwrap();
            assert_eq!(
                server.children.len(),
                1,
                "server should have 1 child before refresh"
            );
        });

        tree.update(cx, |t, cx| {
            t.refresh_server(id, cx);
        });
        cx.run_until_parked();

        tree.read_with(cx, |t, _cx| {
            let server = find_server_node(&t.roots, &sid).unwrap();
            assert_eq!(
                server.children.len(),
                0,
                "server children should be cleared after refresh"
            );
        });
    }

    /// Clicking a server row browses it and nothing more. The click must not
    /// ask the app to connect, because that would move the query tab the user
    /// is running against onto whatever they happened to open in the rail.
    #[gpui::test]
    pub(crate) fn clicking_a_server_row_does_not_request_a_connection(
        cx: &mut gpui::TestAppContext,
    ) {
        let p = {
            use sqlly_core::{SecretProvider, SecretRef};
            ConnectionProfile::sql_login(
                "Prod",
                "host",
                "sa",
                SecretRef {
                    provider: SecretProvider::InMemory,
                    key: "k".into(),
                },
            )
        };
        let id = p.id.0;
        let sid = format!("srv:{id}");

        ensure_components_initialized(cx);
        let (view, cx) = cx.add_window_view(|_window, cx| {
            let wrapper = OffsetTreeWrapper::new(cx);
            wrapper.tree.update(cx, |tree, cx| {
                tree.set_connections(vec![p.clone()], cx);
                // Pre-expanded and loaded, so the click collapses instead of
                // kicking off an async load (which would want a Tokio runtime).
                if let Some(server) = find_server_node_mut(&mut tree.roots, &sid) {
                    server.expanded = true;
                    server.load_state = LoadState::Loaded;
                    server.children = vec![TreeNode::new(
                        format!("{sid}:db:master"),
                        "master",
                        NodeKind::Database("master".into()),
                        1,
                    )];
                }
            });
            wrapper
        });
        cx.run_until_parked();

        // First row (the server) sits at y = 50..50 + row_height, the wrapper's
        // 50px offset plus the tree's own top edge.
        let click_y = px(50.0 + Theme::row_height() / 2.0);
        cx.simulate_mouse_down(
            point(px(110.0), click_y),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.simulate_mouse_up(
            point(px(110.0), click_y),
            gpui::MouseButton::Left,
            gpui::Modifiers::none(),
        );
        cx.run_until_parked();

        let activated = view.read_with(cx, |v, _cx| {
            v.events
                .borrow()
                .iter()
                .filter(|e| matches!(e, SchemaTreeEvent::ActivateServer(_)))
                .count()
        });
        assert_eq!(
            activated, 0,
            "clicking a server row must not emit ActivateServer"
        );
        view.read_with(cx, |v, cx| {
            let tree = v.tree.read(cx);
            assert!(
                tree.active.is_none(),
                "the tree must not mark a server active on a plain click"
            );
            let server = find_server_node(&tree.roots, &sid).unwrap();
            assert!(!server.expanded, "the click should have collapsed the row");
            assert_eq!(
                tree.selected.as_deref(),
                Some(sid.as_str()),
                "the click should still select the row"
            );
        });
    }

    /// `refresh_server` for a server the tree does not know is a no-op — it
    /// should not panic and should leave the tree unchanged.
    #[gpui::test]
    pub(crate) fn refresh_server_noop_for_unknown_server(cx: &mut gpui::TestAppContext) {
        ensure_components_initialized(cx);
        let (tree, cx) = cx.add_window_view(|_window, cx| {
            let services = Services::new();
            let log =
                crate::log_console::DiagnosticLog::new(crate::log_console::DiagnosticLogLevel::Off);
            SchemaTree::new(services, log, cx)
        });

        cx.run_until_parked();

        tree.update(cx, |t, cx| {
            t.refresh_server(Uuid::new_v4(), cx);
        });
        cx.run_until_parked();

        tree.read_with(cx, |t, _cx| {
            assert!(t.roots.is_empty(), "roots should remain empty");
        });
    }

    /// A data-model snapshot with one table (two columns), one view, and one
    /// procedure, all in `dbo` — the shape hydration consumes.
    pub(crate) fn hydration_snapshot() -> sqlly_lang::sqlmodel::ModelSnapshot {
        use sqlly_lang::sqlmodel as m;
        let d = m::Dialect::SqlServer;
        let name = |s: &str| m::SqlName::new(s, d, false);
        let col = |n: &str, ty: &str, ordinal: usize| m::ColumnDef {
            name: name(n),
            ty: Some(m::TypeRef::parse(ty, d)),
            nullable: Some(ordinal != 0),
            default: None,
            identity: ordinal == 0,
            generated: None,
            collation: None,
            ordinal,
        };
        let frag = |n: &str, kind: m::ObjectKind, body: m::ObjectBody| m::ObjectFragment {
            reference: m::ObjectRef::new(None, Some(name("dbo")), name(n)),
            kind,
            dialect: d,
            body,
            provenance: m::Provenance::Database {
                profile: "p".into(),
            },
            fidelity: m::Fidelity::Exact,
            references: vec![],
        };
        let fragments = vec![
            frag(
                "Orders",
                m::ObjectKind::Table,
                m::ObjectBody::Table(m::TableBody {
                    columns: vec![col("Id", "int", 0), col("TotalDue", "money", 1)],
                    ..m::TableBody::default()
                }),
            ),
            frag(
                "OrderSummary",
                m::ObjectKind::View,
                m::ObjectBody::View(m::ViewBody {
                    columns: vec![col("OrderCount", "int", 0)],
                    definition: None,
                }),
            ),
            frag(
                "GetOrders",
                m::ObjectKind::Procedure,
                m::ObjectBody::Routine(m::RoutineBody::default()),
            ),
        ];
        let (snap, _) = m::ModelSnapshot::empty().apply(
            1,
            fragments,
            &std::collections::BTreeSet::new(),
            m::IncompleteRegions::default(),
        );
        snap
    }

    /// Hydration fills an `Unloaded` database node with the snapshot's whole
    /// object tree — ids matching the lazy loader's format — and the filter
    /// finds objects and columns nothing was ever expanded to load.
    #[test]
    pub(crate) fn hydration_populates_unexpanded_db_and_filter_finds_deep_nodes() {
        let snapshot = hydration_snapshot();
        let mut db = TreeNode::new(
            "srv:s:db:mydb",
            "mydb",
            NodeKind::Database("mydb".into()),
            1,
        );
        let hydrated = build_hydrated_db_children(
            &db.id,
            db.depth,
            &snapshot,
            &["dbo".to_string()],
            crate::preferences::DataModelPreferences::default(),
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        );
        merge_hydrated_children(&mut db, hydrated);

        assert!(matches!(db.load_state, LoadState::Loaded));
        // The filter reveals a column of a table that was never expanded.
        let mut rows = Vec::new();
        let matched = flatten_tree_filtered(
            std::slice::from_ref(&db),
            &mut rows,
            None,
            &compile_q("totaldue"),
        );
        assert!(matched, "filter should reach hydrated depth");
        assert!(rows.iter().any(|r| r.title.as_ref() == "TotalDue"));
        // Node ids mirror the lazy loader exactly, so later server loads and
        // expansion state land on the same nodes.
        let mut rows = Vec::new();
        flatten_tree_filtered(
            std::slice::from_ref(&db),
            &mut rows,
            None,
            &compile_q("orders"),
        );
        assert!(
            rows.iter()
                .any(|r| r.id == "srv:s:db:mydb:tables:t:dbo:Orders"),
            "table id should match lazy-loader format"
        );
        assert!(
            rows.iter()
                .any(|r| r.id == "srv:s:db:mydb:prog:p:dbo:GetOrders"),
            "procedure id should match lazy-loader format"
        );
        // Views hydrate with their columns.
        let mut rows = Vec::new();
        flatten_tree_filtered(
            std::slice::from_ref(&db),
            &mut rows,
            None,
            &compile_q("ordercount"),
        );
        assert!(rows.iter().any(|r| r.title == "OrderCount"));
    }

    /// Hydration must never clobber levels the server (or the user's
    /// expansion) already loaded — server truth wins; hydration only fills
    /// `Unloaded` gaps beneath it.
    #[test]
    pub(crate) fn hydration_preserves_server_loaded_children() {
        let snapshot = hydration_snapshot();
        let mut db = TreeNode::new(
            "srv:s:db:mydb",
            "mydb",
            NodeKind::Database("mydb".into()),
            1,
        );
        // The Tables folder was already loaded from the server and contains a
        // table the (stale) snapshot doesn't know.
        let mut tables = TreeNode::new("srv:s:db:mydb:tables", "Tables", NodeKind::TablesFolder, 2);
        tables.load_state = LoadState::Loaded;
        tables.children = vec![TreeNode::new(
            "srv:s:db:mydb:tables:t:dbo:BrandNew",
            "dbo.BrandNew",
            NodeKind::Table {
                schema: "dbo".into(),
                name: "BrandNew".into(),
            },
            3,
        )];
        db.children = vec![tables];
        db.load_state = LoadState::Loaded;

        let hydrated = build_hydrated_db_children(
            &db.id,
            db.depth,
            &snapshot,
            &["dbo".to_string()],
            crate::preferences::DataModelPreferences::default(),
            sqlly_lang::sqlmodel::DatabaseType::SqlServer,
        );
        merge_hydrated_children(&mut db, hydrated);

        let tables = &db.children[0];
        assert_eq!(
            tables.children.len(),
            1,
            "server-loaded folder keeps its own children"
        );
        assert_eq!(tables.children[0].title, "dbo.BrandNew");
    }
}
