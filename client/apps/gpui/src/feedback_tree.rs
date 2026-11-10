//! Data-model disclosure tree for the "improve IntelliSense" feedback dialog.
//!
//! The captured [`FeedbackSnapshot`] holds the *full* completion catalog; this
//! module turns it into a checkable tree (database → schema → object →
//! column/parameter, plus snippets) and filters the snapshot down to exactly
//! what the user opted into before anything leaves the machine. Nothing here
//! touches the UI or the network — it is pure and unit-tested.
//!
//! Migrating the rendering consumer (`intellisense_feedback_dialog`) to
//! gpui-component 0.6.4's `tree` or `list` was evaluated and rejected — the
//! kit widgets cannot express this surface's behavior:
//!
//! - **No row selection, three click targets.** The kit tree/list model is a
//!   single `selected_index` with whole-row click-to-confirm. These rows have
//!   no selection at all; instead each row carries three independent click
//!   targets (chevron = expand, checkbox = tick, label = expand containers /
//!   tick leaves) driving a tri-state Checked/Implied/Unchecked glyph the
//!   stock controls cannot draw.
//! - **Derived visibility, not stored items.** What is listed is recomputed
//!   every render from search (`SearchFilter::visible`), transient
//!   auto-expansion of match ancestors (never mutating manual `expanded`
//!   state), and the per-kind [`KindVisibility`] toggles that drop object
//!   subtrees and column rows. The kit `TreeState` owns a stored, flat item
//!   list with its own expansion state — every keystroke or kind toggle would
//!   mean rebuilding its items and reconciling two expansion sources.
//! - **Mixed row content.** The 400-row cap appends a differently shaped
//!   "… N more" summary row, and include-all renders the whole tree dimmed
//!   and inert; kit rows are uniform-height and stay interactive.
//!
//! The dialog's hand-rolled disclosure list therefore stays; this module
//! remains the pure, tested model behind it.

use std::collections::{BTreeMap, HashSet};

use crate::intellisense_feedback::{CatalogObjectSnapshot, CatalogSnapshot, FeedbackSnapshot};

/// What a tree node stands for. Index fields point back into the snapshot the
/// tree was built from, so filtering never has to re-match by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TreeNodeKind {
    Database,
    Schema,
    Object {
        object_index: usize,
    },
    Column {
        object_index: usize,
        column_index: usize,
    },
    Parameter {
        object_index: usize,
        parameter_index: usize,
    },
    SnippetGroup,
    Snippet {
        snippet_index: usize,
    },
}

#[derive(Debug, Clone)]
pub struct TreeNode {
    pub label: String,
    /// Lowercased label for case-insensitive search.
    pub search_label: String,
    /// Dim suffix shown after the label ("table", "column", ...).
    pub detail: &'static str,
    pub kind: TreeNodeKind,
    pub parent: Option<usize>,
    pub children: Vec<usize>,
    pub depth: usize,
}

/// Arena tree over one captured snapshot. Parents always precede their
/// children in `nodes`, so a single forward pass can propagate top-down state.
#[derive(Debug, Default)]
pub struct FeedbackTree {
    pub nodes: Vec<TreeNode>,
    pub roots: Vec<usize>,
    /// Nodes locked into the report because they back an item the popup
    /// offered: objects flagged `backs_suggestion` and snippets the popup
    /// listed. Always [`CheckState::Checked`] (with everything below them),
    /// cannot be toggled off, and always survive [`Self::filter_snapshot`].
    pub required: HashSet<usize>,
    /// Ancestors of [`Self::required`] nodes — their names always ride along
    /// so a required object has a home (shown as the greyed "name only"
    /// box unless the user checks them whole).
    required_ancestors: HashSet<usize>,
}

/// What the dialog says when the user hovers or clicks a locked row.
pub fn required_explanation() -> String {
    crate::localization::text("feedback.intellisense.required_explanation")
}

/// What the user has opted into. `checked` holds only *explicit* checks; the
/// implied states (ancestors greyed, descendants included) are derived.
#[derive(Debug, Clone, Default)]
pub struct TreeSelection {
    pub include_all: bool,
    pub checked: HashSet<usize>,
}

/// Display state of a node's checkbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckState {
    /// Explicitly checked, under a checked ancestor, or include-all: the node
    /// and everything below it is included.
    Checked,
    /// An ancestor of a checked node: only its *name* is sent, so the checked
    /// descendant has a home. Rendered as the greyed checkbox.
    Implied,
    Unchecked,
}

/// Search result: which nodes stay visible and which must auto-expand so the
/// match is reachable. `None` (from [`FeedbackTree::search`]) means "no query"
/// — show everything, honor manual expansion only.
#[derive(Debug, Default)]
pub struct SearchFilter {
    /// Matches, their ancestors, and everything inside a matched subtree.
    pub visible: HashSet<usize>,
    /// Ancestors of matches — expanded regardless of manual state.
    pub auto_expand: HashSet<usize>,
}

/// Which object kinds the disclosure list shows. Purely a *visibility*
/// filter for the left-hand list: a hidden kind cannot be hand-picked, but
/// column inclusion is never affected by the columns toggle — picking a
/// table always sends all of its columns — and "include all" still sends
/// the full catalog.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KindVisibility {
    pub procedures: bool,
    pub views: bool,
    pub functions: bool,
    pub tables: bool,
    /// Individual column rows under each table/view.
    pub columns: bool,
    pub types: bool,
}

impl Default for KindVisibility {
    fn default() -> Self {
        Self {
            procedures: false,
            views: true,
            functions: false,
            tables: true,
            columns: false,
            types: false,
        }
    }
}

impl KindVisibility {
    /// Whether an object row with this pretty-kind suffix ("table", "view",
    /// "procedure", "function", "type", …) is listed. Kinds without their own
    /// toggle (synonyms, sequences, triggers, generic objects) stay listed.
    pub fn shows_kind(&self, pretty: &str) -> bool {
        match pretty {
            "table" => self.tables,
            "view" => self.views,
            "procedure" => self.procedures,
            "function" => self.functions,
            "type" => self.types,
            _ => true,
        }
    }
}

/// Human kind suffix for a captured object kind (the lowercased `ObjectKind`
/// debug string, e.g. "storedprocedure").
fn pretty_kind(kind: &str) -> &'static str {
    match kind {
        "table" => "table",
        "view" => "view",
        "storedprocedure" => "procedure",
        k if k.contains("function") => "function",
        "type" => "type",
        "synonym" => "synonym",
        "sequence" => "sequence",
        "trigger" => "trigger",
        _ => "object",
    }
}

impl FeedbackTree {
    fn push(
        &mut self,
        label: impl Into<String>,
        detail: &'static str,
        kind: TreeNodeKind,
        parent: Option<usize>,
    ) -> usize {
        let label = label.into();
        let depth = parent.map_or(0, |p| self.nodes[p].depth + 1);
        let index = self.nodes.len();
        self.nodes.push(TreeNode {
            search_label: label.to_lowercase(),
            label,
            detail,
            kind,
            parent,
            children: Vec::new(),
            depth,
        });
        match parent {
            Some(p) => self.nodes[p].children.push(index),
            None => self.roots.push(index),
        }
        index
    }

    /// Builds the disclosure tree from a captured snapshot. Databases only
    /// become their own level when objects actually carry one; loose database
    /// and schema names from the catalog still get (leaf) nodes so the user
    /// can opt into disclosing their existence.
    pub fn build(snapshot: &FeedbackSnapshot) -> Self {
        let catalog = &snapshot.catalog;
        let mut tree = FeedbackTree::default();

        // Group object indices by database then schema, sorted
        // case-insensitively. Key = lowercase, value carries the display name.
        type SchemaMap = BTreeMap<String, (String, Vec<usize>)>;
        let mut by_db: BTreeMap<String, (String, SchemaMap)> = BTreeMap::new();
        for db in &catalog.databases {
            by_db
                .entry(db.to_lowercase())
                .or_insert_with(|| (db.clone(), SchemaMap::new()));
        }
        let mut object_schemas: HashSet<String> = HashSet::new();
        for (i, object) in catalog.objects.iter().enumerate() {
            let db = object.database.as_deref().unwrap_or("");
            let schema = object.schema.as_deref().unwrap_or("");
            object_schemas.insert(schema.to_lowercase());
            by_db
                .entry(db.to_lowercase())
                .or_insert_with(|| (db.to_string(), SchemaMap::new()))
                .1
                .entry(schema.to_lowercase())
                .or_insert_with(|| (schema.to_string(), Vec::new()))
                .1
                .push(i);
        }
        // Loose schema names with no objects anywhere attach under
        // "no database" so their existence is still opt-in-able.
        for schema in &catalog.schemas {
            if object_schemas.contains(&schema.to_lowercase()) {
                continue;
            }
            by_db
                .entry(String::new())
                .or_insert_with(|| (String::new(), SchemaMap::new()))
                .1
                .entry(schema.to_lowercase())
                .or_insert_with(|| (schema.clone(), Vec::new()));
        }

        for (db_display, schemas) in by_db.values() {
            let db_parent = if db_display.is_empty() {
                None
            } else {
                Some(tree.push(db_display.clone(), "database", TreeNodeKind::Database, None))
            };
            for (schema_display, object_indices) in schemas.values() {
                let schema_parent = if schema_display.is_empty() {
                    db_parent
                } else {
                    Some(tree.push(
                        schema_display.clone(),
                        "schema",
                        TreeNodeKind::Schema,
                        db_parent,
                    ))
                };
                // Objects sorted by kind then name so tables/views/routines
                // group naturally inside a schema.
                let mut sorted = object_indices.clone();
                sorted.sort_by_key(|&i| {
                    let o = &catalog.objects[i];
                    (pretty_kind(&o.kind), o.name.to_lowercase())
                });
                for object_index in sorted {
                    let object = &catalog.objects[object_index];
                    let node = tree.push(
                        object.name.clone(),
                        pretty_kind(&object.kind),
                        TreeNodeKind::Object { object_index },
                        schema_parent,
                    );
                    if object.backs_suggestion {
                        tree.required.insert(node);
                    }
                    for (column_index, column) in object.columns.iter().enumerate() {
                        tree.push(
                            column.clone(),
                            "column",
                            TreeNodeKind::Column {
                                object_index,
                                column_index,
                            },
                            Some(node),
                        );
                    }
                    for (parameter_index, parameter) in object.parameters.iter().enumerate() {
                        tree.push(
                            parameter.clone(),
                            "parameter",
                            TreeNodeKind::Parameter {
                                object_index,
                                parameter_index,
                            },
                            Some(node),
                        );
                    }
                }
            }
        }

        if !catalog.snippet_names.is_empty() {
            let group = tree.push(
                crate::localization::text("feedback.intellisense.snippets_group"),
                "",
                TreeNodeKind::SnippetGroup,
                None,
            );
            let offered: HashSet<&str> = snapshot
                .suggestions
                .iter()
                .filter(|s| s.kind == "snippet")
                .map(|s| s.label.as_str())
                .collect();
            for (snippet_index, name) in catalog.snippet_names.iter().enumerate() {
                let node = tree.push(
                    name.clone(),
                    "snippet",
                    TreeNodeKind::Snippet { snippet_index },
                    Some(group),
                );
                if offered.contains(name.as_str()) {
                    tree.required.insert(node);
                }
            }
        }

        for &node in &tree.required {
            let mut current = tree.nodes[node].parent;
            while let Some(p) = current {
                if !tree.required_ancestors.insert(p) {
                    break;
                }
                current = tree.nodes[p].parent;
            }
        }

        tree
    }

    /// Whether a node is locked into the report: it, or an ancestor, backs
    /// an item the popup offered (a required table locks its columns too).
    pub fn is_required(&self, node: usize) -> bool {
        let mut current = Some(node);
        while let Some(n) = current {
            if self.required.contains(&n) {
                return true;
            }
            current = self.nodes[n].parent;
        }
        false
    }

    fn ancestor_checked(&self, selection: &TreeSelection, node: usize) -> bool {
        let mut current = self.nodes[node].parent;
        while let Some(p) = current {
            if selection.checked.contains(&p) {
                return true;
            }
            current = self.nodes[p].parent;
        }
        false
    }

    /// The set of nodes that are ancestors of at least one explicit check —
    /// the "greyed checkbox" set. O(checked × depth).
    pub fn implied_set(&self, selection: &TreeSelection) -> HashSet<usize> {
        let mut implied = self.required_ancestors.clone();
        for &node in &selection.checked {
            let mut current = self.nodes[node].parent;
            while let Some(p) = current {
                if !implied.insert(p) {
                    break;
                }
                current = self.nodes[p].parent;
            }
        }
        implied
    }

    /// Display state of one node's checkbox. `implied` must come from
    /// [`Self::implied_set`] for the same selection.
    pub fn check_state(
        &self,
        selection: &TreeSelection,
        implied: &HashSet<usize>,
        node: usize,
    ) -> CheckState {
        if selection.include_all
            || selection.checked.contains(&node)
            || self.ancestor_checked(selection, node)
            || self.is_required(node)
        {
            CheckState::Checked
        } else if implied.contains(&node) {
            CheckState::Implied
        } else {
            CheckState::Unchecked
        }
    }

    /// The default selection for a statement-scoped capture: the objects the
    /// statement names, checked. The captured catalog is wider than the
    /// statement (a schema mentioned anywhere pulls in every object under it,
    /// so a ticked table always has siblings to broaden into), so seeding
    /// re-matches object names against the SQL: `SELECT * FROM
    /// Accounting.Account` ticks Account, not all of Accounting. When no
    /// object name appears in the SQL (mid-word capture, bare `dbo.`), every
    /// captured object is ticked instead — an untouched dialog must never
    /// send an empty model.
    pub fn default_selection(&self, snapshot: &FeedbackSnapshot) -> HashSet<usize> {
        let sql = format!(
            "{}{}",
            snapshot.sql_before_cursor, snapshot.sql_after_cursor
        );
        let tokens = crate::intellisense_feedback::sql_identifier_tokens(&sql);
        let objects = |named_only: bool| -> HashSet<usize> {
            self.nodes
                .iter()
                .enumerate()
                .filter(|(_, node)| match node.kind {
                    TreeNodeKind::Object { object_index } => {
                        !named_only
                            || tokens.contains(
                                &snapshot.catalog.objects[object_index]
                                    .name
                                    .to_ascii_lowercase(),
                            )
                    }
                    _ => false,
                })
                .map(|(index, _)| index)
                .collect()
        };
        let named = objects(true);
        if named.is_empty() {
            objects(false)
        } else {
            named
        }
    }

    /// Toggles a node's explicit check. Checking a node drops now-redundant
    /// explicit checks on its descendants; a node covered by a checked
    /// ancestor stays checked (uncheck the ancestor to narrow the scope).
    /// A [required](Self::is_required) node is locked: the toggle is refused
    /// and `false` is returned so the caller can explain why.
    pub fn toggle(&self, selection: &mut TreeSelection, node: usize) -> bool {
        if self.is_required(node) {
            return false;
        }
        if selection.checked.remove(&node) {
            return true;
        }
        if self.ancestor_checked(selection, node) {
            return true;
        }
        selection.checked.insert(node);
        let mut stack: Vec<usize> = self.nodes[node].children.clone();
        while let Some(child) = stack.pop() {
            selection.checked.remove(&child);
            stack.extend_from_slice(&self.nodes[child].children);
        }
        true
    }

    /// Case-insensitive substring search. Returns `None` for a blank query.
    pub fn search(&self, query: &str) -> Option<SearchFilter> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return None;
        }
        let mut filter = SearchFilter::default();
        // Forward pass: parents precede children, so "inside a matched
        // subtree" propagates in one sweep.
        let mut in_matched_subtree: Vec<bool> = vec![false; self.nodes.len()];
        for index in 0..self.nodes.len() {
            let node = &self.nodes[index];
            let inherited = node.parent.map(|p| in_matched_subtree[p]).unwrap_or(false);
            let matched = node.search_label.contains(&query);
            in_matched_subtree[index] = inherited || matched;
            if matched {
                filter.visible.insert(index);
                let mut current = node.parent;
                while let Some(p) = current {
                    if !filter.visible.insert(p) && filter.auto_expand.contains(&p) {
                        break;
                    }
                    filter.auto_expand.insert(p);
                    current = self.nodes[p].parent;
                }
            } else if inherited {
                filter.visible.insert(index);
            }
        }
        Some(filter)
    }

    /// Produces the snapshot that actually leaves the machine: the captured
    /// context with the catalog cut down to the user's selection. Include-all
    /// keeps the full catalog (and says so in the payload). Required nodes
    /// (objects backing a popup suggestion, offered snippets) are included
    /// whatever the selection says — [`Self::check_state`] reports them
    /// checked — so a report can never lose the model its popup came from.
    pub fn filter_snapshot(
        &self,
        snapshot: &FeedbackSnapshot,
        selection: &TreeSelection,
    ) -> FeedbackSnapshot {
        let mut filtered = snapshot.clone();
        if selection.include_all {
            filtered.catalog.include_all = true;
            return filtered;
        }

        let implied = self.implied_set(selection);
        let included = |node: usize| -> CheckState { self.check_state(selection, &implied, node) };

        let mut databases = Vec::new();
        let mut schemas = Vec::new();
        let mut objects: Vec<CatalogObjectSnapshot> = Vec::new();
        let mut snippet_names = Vec::new();
        // Names of included objects, for filtering the flat procedure list.
        let mut object_names: HashSet<String> = HashSet::new();

        for (index, node) in self.nodes.iter().enumerate() {
            let state = included(index);
            if state == CheckState::Unchecked {
                continue;
            }
            match &node.kind {
                TreeNodeKind::Database => databases.push(node.label.clone()),
                TreeNodeKind::Schema => schemas.push(node.label.clone()),
                TreeNodeKind::Object { object_index } => {
                    let object = &snapshot.catalog.objects[*object_index];
                    object_names.insert(object.name.to_lowercase());
                    let (columns, parameters) = if state == CheckState::Checked {
                        (object.columns.clone(), object.parameters.clone())
                    } else {
                        // Implied: only the explicitly checked leaves ride
                        // along; the object itself contributes just its name.
                        let mut columns = Vec::new();
                        let mut parameters = Vec::new();
                        for &child in &node.children {
                            if included(child) != CheckState::Checked {
                                continue;
                            }
                            match self.nodes[child].kind {
                                TreeNodeKind::Column { column_index, .. } => {
                                    columns.push(object.columns[column_index].clone());
                                }
                                TreeNodeKind::Parameter {
                                    parameter_index, ..
                                } => {
                                    parameters.push(object.parameters[parameter_index].clone());
                                }
                                _ => {}
                            }
                        }
                        (columns, parameters)
                    };
                    objects.push(CatalogObjectSnapshot {
                        database: object.database.clone(),
                        schema: object.schema.clone(),
                        name: object.name.clone(),
                        kind: object.kind.clone(),
                        columns,
                        parameters,
                        // FKs are unconditional: an object in the report —
                        // picked whole or via any single column — always
                        // carries every foreign key touching it, both
                        // directions. There is no UI to strip them.
                        foreign_keys: object.foreign_keys.clone(),
                        incoming_foreign_keys: object.incoming_foreign_keys.clone(),
                        backs_suggestion: object.backs_suggestion,
                    });
                }
                TreeNodeKind::Snippet { snippet_index } if state == CheckState::Checked => {
                    snippet_names.push(snapshot.catalog.snippet_names[*snippet_index].clone());
                }
                // Column/Parameter inclusion is handled by their object;
                // SnippetGroup by its children; an *implied* snippet leaf
                // cannot exist (leaves have no descendants).
                _ => {}
            }
        }

        let total_objects = objects.len();
        filtered.catalog = CatalogSnapshot {
            databases,
            schemas,
            objects,
            snippet_names,
            total_objects,
            truncated: false,
            include_all: false,
        };
        filtered
            .procedure_names
            .retain(|name| object_names.contains(&name.to_lowercase()));
        filtered
            .procedures
            .retain(|procedure| object_names.contains(&procedure.name.to_lowercase()));
        filtered
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intellisense_feedback::capture_with_full_catalog;
    use sqlly_intel::intellisense::{CompletionCatalog, CompletionObject};
    use sqlly_services::{IntelliSenseRuleConfig, ProcedureInfo};

    fn sample_snapshot() -> FeedbackSnapshot {
        let mut users = CompletionObject::table(
            "dbo",
            "Users",
            vec!["Id".to_string(), "Name".to_string(), "Email".to_string()],
        );
        users.database = Some("appdb".to_string());
        let mut orders = CompletionObject::table(
            "sales",
            "Orders",
            vec!["OrderId".to_string(), "UserId".to_string()],
        );
        orders.database = Some("appdb".to_string());
        let mut get_user = CompletionObject {
            database: Some("appdb".to_string()),
            schema: Some("dbo".to_string()),
            name: "GetUser".to_string(),
            kind: sqlly_core::ObjectKind::StoredProcedure,
            columns: Vec::new(),
            parameters: vec!["@id".to_string()],
            foreign_keys: Vec::new(),
        };
        get_user.parameters = vec!["@id".to_string()];
        let catalog = CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string(), "sales".to_string()],
            objects: vec![users, orders, get_user],
            filesystem_schemas: Vec::new(),
            snippets: vec![("ssf".to_string(), "SELECT * FROM".to_string())],
        };
        let procedures = vec![ProcedureInfo::new("dbo", "GetUser")];
        capture_with_full_catalog(
            "SELECT * FROM dbo.",
            18,
            &[],
            14,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &procedures,
            None,
            false,
            true,
        )
    }

    fn find(tree: &FeedbackTree, label: &str) -> usize {
        tree.nodes
            .iter()
            .position(|n| n.label == label)
            .unwrap_or_else(|| panic!("node {label} not found"))
    }

    #[test]
    fn build_groups_database_schema_object_leaf() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);

        let db = find(&tree, "appdb");
        let dbo = find(&tree, "dbo");
        let users = find(&tree, "Users");
        let email = find(&tree, "Email");
        assert_eq!(tree.nodes[dbo].parent, Some(db));
        assert_eq!(tree.nodes[users].parent, Some(dbo));
        assert_eq!(tree.nodes[email].parent, Some(users));
        assert_eq!(tree.nodes[email].detail, "column");

        // Parents always precede children (single-pass invariant).
        for (i, node) in tree.nodes.iter().enumerate() {
            if let Some(p) = node.parent {
                assert!(p < i);
            }
        }

        // Procedure parameter is a leaf under the procedure.
        let param = find(&tree, "@id");
        let get_user = find(&tree, "GetUser");
        assert_eq!(tree.nodes[param].parent, Some(get_user));
        assert_eq!(tree.nodes[get_user].detail, "procedure");

        // Snippets ride under their own root group.
        let snippet = find(&tree, "ssf");
        let group = find(&tree, "Snippets");
        assert_eq!(tree.nodes[snippet].parent, Some(group));
        assert!(tree.roots.contains(&group));
    }

    #[test]
    fn checking_a_column_implies_table_schema_and_database() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let mut selection = TreeSelection::default();
        let email = find(&tree, "Email");
        tree.toggle(&mut selection, email);

        let implied = tree.implied_set(&selection);
        assert_eq!(
            tree.check_state(&selection, &implied, email),
            CheckState::Checked
        );
        for label in ["Users", "dbo", "appdb"] {
            assert_eq!(
                tree.check_state(&selection, &implied, find(&tree, label)),
                CheckState::Implied,
                "{label} should be implied"
            );
        }
        assert_eq!(
            tree.check_state(&selection, &implied, find(&tree, "Orders")),
            CheckState::Unchecked
        );
    }

    #[test]
    fn checking_a_schema_checks_everything_under_it_and_prunes_redundant_checks() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let mut selection = TreeSelection::default();
        let email = find(&tree, "Email");
        let dbo = find(&tree, "dbo");
        tree.toggle(&mut selection, email);
        tree.toggle(&mut selection, dbo);

        assert!(
            !selection.checked.contains(&email),
            "descendant check pruned"
        );
        let implied = tree.implied_set(&selection);
        for label in ["Users", "Email", "GetUser", "@id"] {
            assert_eq!(
                tree.check_state(&selection, &implied, find(&tree, label)),
                CheckState::Checked,
                "{label} should be checked via the schema"
            );
        }
        // Toggling a covered descendant is a no-op, not an orphan check.
        tree.toggle(&mut selection, email);
        assert!(!selection.checked.contains(&email));
    }

    #[test]
    fn search_expands_ancestors_of_matches_and_keeps_matched_subtrees_visible() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let filter = tree.search("email").expect("non-empty query");

        let email = find(&tree, "Email");
        let users = find(&tree, "Users");
        let dbo = find(&tree, "dbo");
        let db = find(&tree, "appdb");
        assert!(filter.visible.contains(&email));
        for ancestor in [users, dbo, db] {
            assert!(filter.visible.contains(&ancestor));
            assert!(filter.auto_expand.contains(&ancestor));
        }
        assert!(!filter.visible.contains(&find(&tree, "Orders")));

        // A match on a container keeps its whole subtree reachable.
        let filter = tree.search("users").expect("non-empty query");
        assert!(filter.visible.contains(&find(&tree, "Email")));
        assert!(!filter.auto_expand.contains(&users));

        assert!(tree.search("   ").is_none());
    }

    #[test]
    fn filter_sends_only_checked_names_and_implied_existence() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let mut selection = TreeSelection::default();
        tree.toggle(&mut selection, find(&tree, "Email"));

        let filtered = tree.filter_snapshot(&snapshot, &selection);
        assert_eq!(filtered.catalog.databases, vec!["appdb"]);
        assert_eq!(filtered.catalog.schemas, vec!["dbo"]);
        assert_eq!(filtered.catalog.objects.len(), 1);
        let object = &filtered.catalog.objects[0];
        assert_eq!(object.name, "Users");
        assert_eq!(object.columns, vec!["Email"]);
        assert!(object.parameters.is_empty());
        assert!(filtered.catalog.snippet_names.is_empty());
        assert_eq!(filtered.catalog.total_objects, 1);
        assert!(!filtered.catalog.truncated);
        assert!(!filtered.catalog.include_all);
        // GetUser was not opted in, so the flat procedure list drops it too.
        assert!(filtered.procedure_names.is_empty());
        // The rest of the capture is untouched.
        assert_eq!(filtered.sql_before_cursor, snapshot.sql_before_cursor);
    }

    /// When the SQL names none of the captured objects (bare `dbo.` here),
    /// the default selection falls back to every captured object, so an
    /// untouched dialog sends the model the capture scoped to — not an empty
    /// one.
    #[test]
    fn default_selection_falls_back_to_every_captured_object() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let selection = TreeSelection {
            include_all: false,
            checked: tree.default_selection(&snapshot),
        };

        let filtered = tree.filter_snapshot(&snapshot, &selection);
        let mut names: Vec<&str> = filtered
            .catalog
            .objects
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["GetUser", "Orders", "Users"]);
        // Checked objects carry their full column/parameter lists.
        let users = filtered
            .catalog
            .objects
            .iter()
            .find(|o| o.name == "Users")
            .unwrap();
        assert_eq!(users.columns, vec!["Id", "Name", "Email"]);
        // Snippets are not part of the referenced model — not pre-checked.
        assert!(filtered.catalog.snippet_names.is_empty());
        assert!(!filtered.catalog.include_all);
    }

    /// The default selection ticks exactly the objects the statement names,
    /// even though mentioning a schema pulls every object under it into the
    /// captured catalog: `Accounting.Account` must tick Account, not the
    /// whole Accounting schema.
    #[test]
    fn default_selection_ticks_only_statement_named_objects() {
        let account = CompletionObject::table("Accounting", "Account", vec!["Name".to_string()]);
        let bank = CompletionObject::table("Accounting", "Bank", vec!["Id".to_string()]);
        let item_category =
            CompletionObject::table("Inventory", "ItemCategory", vec!["Id".to_string()]);
        let item = CompletionObject::table("Inventory", "Item", vec!["Id".to_string()]);
        let catalog = CompletionCatalog {
            databases: Vec::new(),
            schemas: vec!["Accounting".to_string(), "Inventory".to_string()],
            objects: vec![account, bank, item_category, item],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let sql = "SELECT * FROM Inventory.ItemCategory\n\n\
                   SELECT * FROM Accounting.Account where Name like ";
        let snapshot = capture_with_full_catalog(
            sql,
            sql.len(),
            &[],
            sql.len(),
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            false,
        );
        let tree = FeedbackTree::build(&snapshot);
        let selection = TreeSelection {
            include_all: false,
            checked: tree.default_selection(&snapshot),
        };

        let filtered = tree.filter_snapshot(&snapshot, &selection);
        let mut names: Vec<&str> = filtered
            .catalog
            .objects
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        names.sort_unstable();
        assert_eq!(names, vec!["Account", "ItemCategory"]);
    }

    /// Picking a single column of a table still ships every FK touching that
    /// table — the FK inclusion rule is unconditional for included tables.
    #[test]
    fn filter_keeps_foreign_keys_for_a_table_included_via_one_column() {
        use sqlly_intel::intellisense::FkRelation;

        let mut users =
            CompletionObject::table("dbo", "Users", vec!["Id".to_string(), "Email".to_string()]);
        users.database = Some("appdb".to_string());
        let mut orders = CompletionObject::table(
            "dbo",
            "Orders",
            vec!["OrderId".to_string(), "UserId".to_string()],
        );
        orders.database = Some("appdb".to_string());
        orders.foreign_keys = vec![FkRelation {
            column: "UserId".to_string(),
            target_schema: "dbo".to_string(),
            target_table: "Users".to_string(),
            target_column: "Id".to_string(),
            constraint: None,
        }];
        let catalog = CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string()],
            objects: vec![users, orders],
            filesystem_schemas: Vec::new(),
            snippets: Vec::new(),
        };
        let snapshot = capture_with_full_catalog(
            "SELECT",
            0,
            &[],
            0,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        let tree = FeedbackTree::build(&snapshot);
        let mut selection = TreeSelection::default();
        tree.toggle(&mut selection, find(&tree, "Email"));

        let filtered = tree.filter_snapshot(&snapshot, &selection);
        assert_eq!(filtered.catalog.objects.len(), 1);
        let object = &filtered.catalog.objects[0];
        assert_eq!(object.name, "Users");
        assert_eq!(object.columns, vec!["Email"]);
        assert_eq!(
            object.incoming_foreign_keys.len(),
            1,
            "the incoming FK from Orders must ride along"
        );
        assert_eq!(object.incoming_foreign_keys[0].source_table, "Orders");
    }

    /// Pins the ObjectKind → pretty-kind → visibility-toggle chain for every
    /// variant. The match is exhaustive on purpose: adding an ObjectKind
    /// variant fails to compile here until its mapping is decided, so a kind
    /// toggle can never silently stop covering an object kind because a
    /// Debug string drifted.
    #[test]
    fn kind_visibility_covers_every_object_kind() {
        use sqlly_core::ObjectKind as K;

        fn expected_pretty(kind: &K) -> &'static str {
            match kind {
                K::Table => "table",
                K::View => "view",
                K::StoredProcedure => "procedure",
                K::ScalarFunction
                | K::TableFunction
                | K::AggregateFunction
                | K::PartitionFunction => "function",
                K::Type => "type",
                K::Synonym => "synonym",
                K::Sequence => "sequence",
                K::Trigger => "trigger",
                // Everything else renders as a generic object row that the
                // kind toggles deliberately leave visible.
                K::Server
                | K::Database
                | K::Schema
                | K::Index
                | K::Statistic
                | K::Constraint
                | K::Column
                | K::Key
                | K::User
                | K::Role
                | K::Login
                | K::Filegroup
                | K::PartitionScheme
                | K::Rule
                | K::Default
                | K::Extension
                | K::Event
                | K::MaterializedView
                | K::Assembly
                | K::Job
                | K::Schedule
                | K::Operator
                | K::Alert
                | K::Endpoint
                | K::LinkedServer
                | K::Credential
                | K::Certificate
                | K::SymmetricKey
                | K::AsymmetricKey
                | K::Service
                | K::Queue
                | K::Contract
                | K::MessageType
                | K::Route
                | K::BrokerPriority
                | K::ErrorLog => "object",
            }
        }

        let all = [
            K::Server,
            K::Database,
            K::Schema,
            K::Table,
            K::View,
            K::StoredProcedure,
            K::ScalarFunction,
            K::TableFunction,
            K::AggregateFunction,
            K::Trigger,
            K::Index,
            K::Statistic,
            K::Constraint,
            K::Column,
            K::Key,
            K::User,
            K::Role,
            K::Login,
            K::Filegroup,
            K::PartitionScheme,
            K::PartitionFunction,
            K::Synonym,
            K::Type,
            K::Rule,
            K::Default,
            K::Sequence,
            K::Extension,
            K::Event,
            K::MaterializedView,
            K::Assembly,
            K::Job,
            K::Schedule,
            K::Operator,
            K::Alert,
            K::Endpoint,
            K::LinkedServer,
            K::Credential,
            K::Certificate,
            K::SymmetricKey,
            K::AsymmetricKey,
            K::Service,
            K::Queue,
            K::Contract,
            K::MessageType,
            K::Route,
            K::BrokerPriority,
            K::ErrorLog,
        ];
        let defaults = KindVisibility::default();
        for kind in &all {
            // The snapshot stores the lowercased Debug string — the exact
            // path the capture takes.
            let stored = format!("{kind:?}").to_ascii_lowercase();
            let pretty = pretty_kind(&stored);
            assert_eq!(
                pretty,
                expected_pretty(kind),
                "pretty_kind drifted for {kind:?} (stored as {stored:?})"
            );
            let shown = defaults.shows_kind(pretty);
            let expected_shown = match pretty {
                "table" | "view" => true,
                "procedure" | "function" | "type" => false,
                // Kinds without a toggle always stay listed.
                _ => true,
            };
            assert_eq!(
                shown, expected_shown,
                "default visibility drifted for {kind:?} (pretty {pretty:?})"
            );
        }
    }

    #[test]
    fn filter_with_nothing_checked_sends_an_empty_catalog() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let selection = TreeSelection::default();
        let filtered = tree.filter_snapshot(&snapshot, &selection);
        assert!(filtered.catalog.databases.is_empty());
        assert!(filtered.catalog.schemas.is_empty());
        assert!(filtered.catalog.objects.is_empty());
        assert!(filtered.catalog.snippet_names.is_empty());
        assert!(filtered.procedure_names.is_empty());
    }

    #[test]
    fn checked_table_sends_all_columns_and_keeps_matching_procedures() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let mut selection = TreeSelection::default();
        tree.toggle(&mut selection, find(&tree, "GetUser"));

        let filtered = tree.filter_snapshot(&snapshot, &selection);
        let object = &filtered.catalog.objects[0];
        assert_eq!(object.name, "GetUser");
        assert_eq!(object.parameters, vec!["@id"]);
        assert_eq!(filtered.procedure_names, vec!["GetUser"]);
    }

    #[test]
    fn include_all_keeps_the_full_catalog_and_flags_it() {
        let snapshot = sample_snapshot();
        let tree = FeedbackTree::build(&snapshot);
        let selection = TreeSelection {
            include_all: true,
            checked: HashSet::new(),
        };
        let filtered = tree.filter_snapshot(&snapshot, &selection);
        assert_eq!(
            filtered.catalog.objects.len(),
            snapshot.catalog.objects.len()
        );
        assert!(filtered.catalog.include_all);
        assert_eq!(filtered.procedure_names, snapshot.procedure_names);
    }

    /// The sample model, captured with a popup that offered the `OrderId`
    /// column (backed by sales.Orders) and the `ssf` snippet.
    fn snapshot_with_offered_items() -> FeedbackSnapshot {
        use sqlly_intel::intellisense::{CompletionItem, CompletionKind, CompletionSource};
        let offered = |label: &str, kind, source| CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            kind,
            source,
            score: 70,
            detail: String::new(),
            replace_span: None,
        };
        let mut users = CompletionObject::table(
            "dbo",
            "Users",
            vec!["Id".to_string(), "Name".to_string(), "Email".to_string()],
        );
        users.database = Some("appdb".to_string());
        let mut orders = CompletionObject::table(
            "sales",
            "Orders",
            vec!["OrderId".to_string(), "UserId".to_string()],
        );
        orders.database = Some("appdb".to_string());
        let catalog = CompletionCatalog {
            databases: vec!["appdb".to_string()],
            schemas: vec!["dbo".to_string(), "sales".to_string()],
            objects: vec![users, orders],
            filesystem_schemas: Vec::new(),
            snippets: vec![("ssf".to_string(), "SELECT * FROM".to_string())],
        };
        let snapshot = capture_with_full_catalog(
            "SELECT ",
            7,
            &[
                offered(
                    "OrderId",
                    CompletionKind::Column,
                    CompletionSource::LiveMetadata,
                ),
                offered("ssf", CompletionKind::Snippet, CompletionSource::Snippet),
            ],
            7,
            0,
            &IntelliSenseRuleConfig::default(),
            &catalog,
            &[],
            None,
            false,
            true,
        );
        assert_eq!(snapshot.catalog.objects.len(), 2);
        snapshot
    }

    #[test]
    fn suggestion_backing_objects_are_locked_checked() {
        let snapshot = snapshot_with_offered_items();
        let tree = FeedbackTree::build(&snapshot);
        let orders = find(&tree, "Orders");
        let order_id = find(&tree, "OrderId");
        let users = find(&tree, "Users");
        let snippet = find(&tree, "ssf");
        let sales = find(&tree, "sales");

        let mut selection = TreeSelection::default();
        let implied = tree.implied_set(&selection);
        assert_eq!(
            tree.check_state(&selection, &implied, orders),
            CheckState::Checked
        );
        assert_eq!(
            tree.check_state(&selection, &implied, order_id),
            CheckState::Checked
        );
        assert_eq!(
            tree.check_state(&selection, &implied, snippet),
            CheckState::Checked
        );
        assert_eq!(
            tree.check_state(&selection, &implied, sales),
            CheckState::Implied
        );
        assert_eq!(
            tree.check_state(&selection, &implied, users),
            CheckState::Unchecked
        );

        // Unticking a locked row (or one of its columns) is refused.
        assert!(!tree.toggle(&mut selection, orders));
        assert!(!tree.toggle(&mut selection, order_id));
        assert!(!tree.toggle(&mut selection, snippet));
        assert!(selection.checked.is_empty());
        // Ordinary rows still toggle.
        assert!(tree.toggle(&mut selection, users));
        assert!(selection.checked.contains(&users));
    }

    /// Enforcement lives in the data path, not just the UI: whatever is
    /// (un)ticked, the submitted snapshot carries every suggestion-backing
    /// object whole, plus the offered snippet.
    #[test]
    fn filter_always_sends_suggestion_backing_objects() {
        let snapshot = snapshot_with_offered_items();
        let tree = FeedbackTree::build(&snapshot);

        // Nothing ticked at all.
        let filtered = tree.filter_snapshot(&snapshot, &TreeSelection::default());
        assert_eq!(filtered.catalog.objects.len(), 1);
        let orders = &filtered.catalog.objects[0];
        assert_eq!(orders.name, "Orders");
        assert!(orders.backs_suggestion);
        assert_eq!(orders.columns, vec!["OrderId", "UserId"]);
        assert_eq!(filtered.catalog.schemas, vec!["sales"]);
        assert_eq!(filtered.catalog.databases, vec!["appdb"]);
        assert_eq!(filtered.catalog.snippet_names, vec!["ssf"]);

        // A hand-built selection that tries to exclude it still sends it.
        let selection = TreeSelection {
            include_all: false,
            checked: [find(&tree, "Users")].into_iter().collect(),
        };
        let filtered = tree.filter_snapshot(&snapshot, &selection);
        let mut names: Vec<&str> = filtered
            .catalog
            .objects
            .iter()
            .map(|o| o.name.as_str())
            .collect();
        names.sort();
        assert_eq!(names, ["Orders", "Users"]);
    }
}
