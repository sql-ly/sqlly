//! Deploy carts: named, persisted collections of objects gathered from any
//! connection, deployed as one dependency-ordered script or per-object files.
//!
//! The cart is pure state: identity, grouping by source connection, and the
//! handoff into the script pipeline ([`ScriptSetObject`] lists for the
//! in-memory CREATE path, [`DumpOptions`] filters for the file-based dump
//! path). Where the JSON file lives is the host layer's business.

use serde::{Deserialize, Serialize};
use sqlly_core::ObjectKind;
use uuid::Uuid;

use crate::dump_render::table_key;
use crate::dump_render::{DumpContent, DumpLayout, DumpOptions};
use crate::script_set::ScriptSetObject;

/// The object kinds a cart can hold. Mirrors the schema tree's scriptable
/// object kinds without depending on any UI type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeployObjectKind {
    Table,
    View,
    Procedure,
    Function,
}

impl DeployObjectKind {
    pub fn label(self) -> &'static str {
        match self {
            DeployObjectKind::Table => "Table",
            DeployObjectKind::View => "View",
            DeployObjectKind::Procedure => "Procedure",
            DeployObjectKind::Function => "Function",
        }
    }

    pub fn message(self) -> sqlly_core::DisplayMessage {
        let (key, fallback) = match self {
            Self::Table => ("services.deploy_cart.kind_table", "Table"),
            Self::View => ("services.deploy_cart.kind_view", "View"),
            Self::Procedure => ("services.deploy_cart.kind_procedure", "Procedure"),
            Self::Function => ("services.deploy_cart.kind_function", "Function"),
        };
        sqlly_core::DisplayMessage::new(key, fallback)
    }

    pub fn object_kind(self) -> ObjectKind {
        match self {
            DeployObjectKind::Table => ObjectKind::Table,
            DeployObjectKind::View => ObjectKind::View,
            DeployObjectKind::Procedure => ObjectKind::StoredProcedure,
            DeployObjectKind::Function => ObjectKind::ScalarFunction,
        }
    }
}

/// One collected object. `include_data` asks the deploy to carry the table's
/// rows as INSERT batches alongside its structure (tables only).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployCartItem {
    pub server_id: Uuid,
    /// Connection display name at collection time, for grouping headers —
    /// a rename later does not rewrite stored carts.
    #[serde(default)]
    pub server_name: String,
    #[serde(default)]
    pub database: String,
    #[serde(default)]
    pub schema: String,
    pub name: String,
    pub kind: DeployObjectKind,
    #[serde(default)]
    pub include_data: bool,
}

impl DeployCartItem {
    /// The dedupe identity: same object on the same connection/database,
    /// regardless of display name or data flag.
    fn same_object(&self, other: &DeployCartItem) -> bool {
        self.server_id == other.server_id
            && self.database == other.database
            && self.schema == other.schema
            && self.name == other.name
            && self.kind == other.kind
    }

    /// Display label (`schema.name`, or bare `name` when schema-less).
    pub fn label(&self) -> String {
        table_key(&self.schema, &self.name)
    }

    pub fn script_object(&self) -> ScriptSetObject {
        ScriptSetObject::new(&self.schema, &self.name, self.kind.object_kind())
    }
}

/// What [`DeployCart::add`] did with an item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddOutcome {
    /// The object was not in the cart; it is now.
    Added,
    /// The object was already in the cart structure-only; re-adding it with
    /// data upgraded the existing entry.
    UpgradedWithData,
    /// The object was already in the cart; nothing changed.
    Duplicate,
}

/// A named collection of objects, deployed together.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployCart {
    pub id: Uuid,
    pub name: String,
    #[serde(default)]
    pub items: Vec<DeployCartItem>,
}

impl DeployCart {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            id: Uuid::new_v4(),
            name: name.into(),
            items: Vec::new(),
        }
    }

    /// Add an object, deduplicated by identity. Re-adding with `include_data`
    /// upgrades the existing entry rather than listing the object twice.
    pub fn add(&mut self, item: DeployCartItem) -> AddOutcome {
        if let Some(existing) = self.items.iter_mut().find(|e| e.same_object(&item)) {
            if item.include_data && !existing.include_data {
                existing.include_data = true;
                return AddOutcome::UpgradedWithData;
            }
            return AddOutcome::Duplicate;
        }
        self.items.push(item);
        AddOutcome::Added
    }

    pub fn remove_at(&mut self, index: usize) -> Option<DeployCartItem> {
        (index < self.items.len()).then(|| self.items.remove(index))
    }

    /// Move an item from `from` to `to` (both clamped), for manual ordering.
    pub fn move_item(&mut self, from: usize, to: usize) -> bool {
        if self.items.is_empty() {
            return false;
        }
        let last = self.items.len() - 1;
        let from = from.min(last);
        let to = to.min(last);
        if from == to {
            return false;
        }
        let item = self.items.remove(from);
        self.items.insert(to, item);
        true
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// The cart's contents grouped by source connection + database, in first
    /// appearance order; items keep cart order within each group. Script
    /// generation works one group at a time — a script is always against a
    /// single source database.
    pub fn groups(&self) -> Vec<DeployCartGroup> {
        let mut groups: Vec<DeployCartGroup> = Vec::new();
        for item in &self.items {
            match groups
                .iter_mut()
                .find(|g| g.server_id == item.server_id && g.database == item.database)
            {
                Some(group) => group.items.push(item.clone()),
                None => groups.push(DeployCartGroup {
                    server_id: item.server_id,
                    server_name: item.server_name.clone(),
                    database: item.database.clone(),
                    items: vec![item.clone()],
                }),
            }
        }
        groups
    }
}

/// The slice of a cart that lives on one connection + database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeployCartGroup {
    pub server_id: Uuid,
    pub server_name: String,
    pub database: String,
    pub items: Vec<DeployCartItem>,
}

impl DeployCartGroup {
    pub fn any_include_data(&self) -> bool {
        self.items.iter().any(|i| i.include_data)
    }

    /// Qualified labels for `DumpOptions.objects` (the structure scope).
    pub fn object_labels(&self) -> Vec<String> {
        self.items.iter().map(|i| i.label()).collect()
    }

    /// Qualified labels of the items flagged include-data, for
    /// `DumpOptions.data_objects`.
    pub fn data_labels(&self) -> Vec<String> {
        self.items
            .iter()
            .filter(|i| i.include_data)
            .map(|i| i.label())
            .collect()
    }

    /// The group's objects for the in-memory CREATE-script path.
    pub fn script_objects(&self) -> Vec<ScriptSetObject> {
        self.items.iter().map(|i| i.script_object()).collect()
    }

    /// Dump options for the group's flagged tables' rows only (INSERT
    /// batches, no structure), or `None` when nothing is flagged. The
    /// generated script's structure comes from the script-set pipeline, so
    /// the cart order and definition dependencies decide it; the dump
    /// pipeline supplies only the data, parents before children by FK.
    pub fn data_only_dump_options(&self) -> Option<DumpOptions> {
        self.any_include_data().then(|| DumpOptions {
            content: DumpContent::DataOnly,
            layout: DumpLayout::SingleFile,
            objects: Some(self.data_labels()),
            data_objects: None,
            ..DumpOptions::default()
        })
    }

    /// Dump options scoped to exactly this group: object-filtered, structure
    /// only unless something is flagged for data, in the caller's layout.
    pub fn dump_options(&self, layout: DumpLayout) -> DumpOptions {
        let with_data = self.any_include_data();
        DumpOptions {
            content: if with_data {
                DumpContent::StructureAndData
            } else {
                DumpContent::StructureOnly
            },
            layout,
            objects: Some(self.object_labels()),
            data_objects: with_data.then(|| self.data_labels()),
            ..DumpOptions::default()
        }
    }
}

/// Current schema version written by [`DeployCartsFile::to_json`].
pub const DEPLOY_CARTS_VERSION: u32 = 1;

/// The on-disk cart collection: a versioned JSON document so future schema
/// changes can migrate old files instead of dropping them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeployCartsFile {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub carts: Vec<DeployCart>,
    /// The cart "Add to Deploy Cart" appends to. `None` until anything is
    /// collected; a dangling id (cart deleted) heals on the next add.
    #[serde(default)]
    pub active_cart: Option<Uuid>,
}

impl Default for DeployCartsFile {
    fn default() -> Self {
        Self {
            version: DEPLOY_CARTS_VERSION,
            carts: Vec::new(),
            active_cart: None,
        }
    }
}

impl DeployCartsFile {
    /// Tolerant parse: unreadable or partial JSON yields an empty collection
    /// rather than an error (a corrupt file must not break the explorer).
    pub fn from_json(text: &str) -> Self {
        Self::try_from_json(text).unwrap_or_default()
    }

    /// Strict parse for persistence paths: corrupt JSON is an error so saves
    /// cannot overwrite a damaged file with an empty default.
    pub fn try_from_json(text: &str) -> Result<Self, String> {
        serde_json::from_str::<Self>(text).map_err(|e| e.to_string())
    }

    /// Pretty-printed JSON for persisting; the version field is always the
    /// current [`DEPLOY_CARTS_VERSION`].
    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(&Self {
            version: DEPLOY_CARTS_VERSION,
            carts: self.carts.clone(),
            active_cart: self.active_cart,
        })
        .unwrap_or_else(|_| "{}".to_string())
    }

    /// The cart new items land in, creating the default cart when the file is
    /// empty or the recorded active cart no longer exists.
    pub fn active_mut(&mut self) -> &mut DeployCart {
        let valid = self
            .active_cart
            .is_some_and(|id| self.carts.iter().any(|c| c.id == id));
        if !valid {
            let cart = DeployCart::new("Default");
            self.active_cart = Some(cart.id);
            self.carts.push(cart);
        }
        let id = self.active_cart.expect("active cart was just ensured");
        self.carts
            .iter_mut()
            .find(|c| c.id == id)
            .expect("active cart id is present")
    }

    pub fn cart_mut(&mut self, id: Uuid) -> Option<&mut DeployCart> {
        self.carts.iter_mut().find(|c| c.id == id)
    }

    /// Whether another cart (any but `except`) already uses `name`, ignoring
    /// case and surrounding whitespace. Cart names are unique so the picker
    /// and status lines can never confuse two carts.
    pub fn name_taken(&self, name: &str, except: Option<Uuid>) -> bool {
        let wanted = name.trim().to_lowercase();
        self.carts
            .iter()
            .any(|c| Some(c.id) != except && c.name.trim().to_lowercase() == wanted)
    }

    /// `base`, or `base 2`, `base 3`, … — the first name no cart uses.
    pub fn unique_cart_name(&self, base: &str) -> String {
        if !self.name_taken(base, None) {
            return base.to_string();
        }
        (2..)
            .map(|n| format!("{base} {n}"))
            .find(|candidate| !self.name_taken(candidate, None))
            .expect("an unused suffix always exists")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(
        server: Uuid,
        database: &str,
        schema: &str,
        name: &str,
        kind: DeployObjectKind,
    ) -> DeployCartItem {
        DeployCartItem {
            server_id: server,
            server_name: format!("srv-{server}"),
            database: database.to_string(),
            schema: schema.to_string(),
            name: name.to_string(),
            kind,
            include_data: false,
        }
    }

    #[test]
    fn add_dedupes_and_data_readd_upgrades() {
        let server = Uuid::new_v4();
        let mut cart = DeployCart::new("A");
        assert_eq!(
            cart.add(item(
                server,
                "shop",
                "dbo",
                "orders",
                DeployObjectKind::Table
            )),
            AddOutcome::Added
        );
        assert_eq!(
            cart.add(item(
                server,
                "shop",
                "dbo",
                "orders",
                DeployObjectKind::Table
            )),
            AddOutcome::Duplicate
        );
        let mut with_data = item(server, "shop", "dbo", "orders", DeployObjectKind::Table);
        with_data.include_data = true;
        assert_eq!(cart.add(with_data), AddOutcome::UpgradedWithData);
        assert_eq!(cart.items.len(), 1);
        assert!(cart.items[0].include_data);
        // Once data-flagged, a plain re-add changes nothing.
        assert_eq!(
            cart.add(item(
                server,
                "shop",
                "dbo",
                "orders",
                DeployObjectKind::Table
            )),
            AddOutcome::Duplicate
        );
        // A same-named object of a different kind is a different entry.
        assert_eq!(
            cart.add(item(
                server,
                "shop",
                "dbo",
                "orders",
                DeployObjectKind::View
            )),
            AddOutcome::Added
        );
        assert_eq!(cart.items.len(), 2);
    }

    #[test]
    fn remove_and_move_keep_cart_order_sane() {
        let server = Uuid::new_v4();
        let mut cart = DeployCart::new("A");
        for name in ["a", "b", "c"] {
            cart.add(item(server, "db", "dbo", name, DeployObjectKind::Table));
        }
        assert!(cart.move_item(0, 2));
        let names: Vec<&str> = cart.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["b", "c", "a"]);
        assert!(!cart.move_item(2, 2));
        assert!(
            !cart.move_item(2, 99),
            "an out-of-range target clamps to the same slot"
        );
        assert!(
            cart.move_item(0, 99),
            "out-of-range targets clamp to the end"
        );
        let names: Vec<&str> = cart.items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, vec!["c", "a", "b"]);
        assert_eq!(cart.remove_at(0).unwrap().name, "c");
        assert_eq!(cart.remove_at(9), None);
        cart.clear();
        assert!(cart.items.is_empty());
        assert!(!cart.move_item(0, 1));
    }

    #[test]
    fn groups_follow_first_appearance_and_keep_cart_order() {
        let s1 = Uuid::new_v4();
        let s2 = Uuid::new_v4();
        let mut cart = DeployCart::new("A");
        cart.add(item(s1, "shop", "dbo", "a", DeployObjectKind::Table));
        cart.add(item(s2, "analytics", "dbo", "b", DeployObjectKind::Table));
        cart.add(item(s1, "shop", "dbo", "c", DeployObjectKind::Table));
        cart.add(item(s1, "other", "dbo", "d", DeployObjectKind::Table));
        let groups = cart.groups();
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].database, "shop");
        assert_eq!(groups[0].items.len(), 2);
        assert_eq!(groups[0].items[1].name, "c");
        assert_eq!(groups[1].database, "analytics");
        assert_eq!(groups[2].database, "other");
    }

    #[test]
    fn dump_options_scope_structure_and_data_to_the_group() {
        let server = Uuid::new_v4();
        let mut cart = DeployCart::new("A");
        cart.add(item(
            server,
            "shop",
            "dbo",
            "orders",
            DeployObjectKind::Table,
        ));
        cart.add(item(
            server,
            "shop",
            "dbo",
            "v_orders",
            DeployObjectKind::View,
        ));
        let mut with_data = item(server, "shop", "sales", "lines", DeployObjectKind::Table);
        with_data.include_data = true;
        cart.add(with_data);
        let group = cart.groups().remove(0);

        // Structure only until something asks for data…
        let structure = group.dump_options(DumpLayout::SingleFile);
        assert_eq!(
            structure.content,
            DumpContent::StructureAndData,
            "one item is flagged"
        );
        assert_eq!(
            structure.objects.as_ref().unwrap(),
            &vec!["dbo.orders", "dbo.v_orders", "sales.lines"]
        );
        assert_eq!(
            structure.data_objects.as_ref().unwrap(),
            &vec!["sales.lines"]
        );

        // …and with no flags the data scope stays closed.
        let mut plain = DeployCart::new("B");
        plain.add(item(
            server,
            "shop",
            "dbo",
            "orders",
            DeployObjectKind::Table,
        ));
        let opts = plain
            .groups()
            .remove(0)
            .dump_options(DumpLayout::FilePerObject);
        assert_eq!(opts.content, DumpContent::StructureOnly);
        assert!(opts.data_objects.is_none());
        assert_eq!(opts.layout, DumpLayout::FilePerObject);
    }

    #[test]
    fn carts_file_round_trips_and_tolerates_old_json() {
        let server = Uuid::new_v4();
        let mut file = DeployCartsFile::default();
        file.active_mut().add(item(
            server,
            "shop",
            "dbo",
            "orders",
            DeployObjectKind::Table,
        ));
        let json = file.to_json();
        let restored = DeployCartsFile::from_json(&json);
        assert_eq!(restored.version, DEPLOY_CARTS_VERSION);
        assert_eq!(restored, file);
        // Garbage → empty collection for display, error for strict saves.
        assert!(DeployCartsFile::from_json("not json").carts.is_empty());
        assert!(DeployCartsFile::try_from_json("not json").is_err());
        // A pre-cart-fields file loads with defaults.
        let old = r#"{"version":1,"carts":[{"id":null,"name":"Old","items":[]}]}"#;
        assert!(
            DeployCartsFile::try_from_json(old).is_err(),
            "uuid must parse"
        );
        let sparse = format!(
            r#"{{"version":1,"carts":[{{"id":"{}","name":"Old"}}]}}"#,
            Uuid::new_v4()
        );
        let file = DeployCartsFile::from_json(&sparse);
        assert_eq!(file.carts.len(), 1);
        assert!(file.carts[0].items.is_empty());
        assert!(file.active_cart.is_none());
    }

    #[test]
    fn active_cart_is_created_and_heals_after_deletion() {
        let mut file = DeployCartsFile::default();
        let first_id = file.active_mut().id;
        assert_eq!(file.carts.len(), 1);
        assert_eq!(file.active_cart, Some(first_id));
        // Same active cart on the next add — no duplicate defaults.
        file.active_mut().name = "Renamed".to_string();
        assert_eq!(file.carts.len(), 1);
        // A dangling active id heals into a fresh default cart.
        file.carts.clear();
        let healed = file.active_mut().id;
        assert_ne!(healed, first_id);
        assert_eq!(file.active_cart, Some(healed));
    }

    #[test]
    fn cart_names_are_unique_ignoring_case() {
        let mut file = DeployCartsFile::default();
        let first = file.active_mut().id;
        assert_eq!(file.carts[0].name, "Default");
        assert!(file.name_taken(" default ", None));
        assert!(
            !file.name_taken("Default", Some(first)),
            "a cart may keep its own name"
        );
        assert_eq!(file.unique_cart_name("Default"), "Default 2");
        file.carts.push(DeployCart::new("Default 2"));
        assert_eq!(file.unique_cart_name("Default"), "Default 3");
        assert_eq!(file.unique_cart_name("Staging"), "Staging");
    }

    #[test]
    fn data_only_options_cover_just_the_flagged_tables() {
        let server = Uuid::new_v4();
        let mut cart = DeployCart::new("A");
        cart.add(item(
            server,
            "shop",
            "dbo",
            "orders",
            DeployObjectKind::Table,
        ));
        assert!(cart.groups()[0].data_only_dump_options().is_none());
        let mut lines = item(server, "shop", "sales", "lines", DeployObjectKind::Table);
        lines.include_data = true;
        cart.add(lines);
        let opts = cart.groups()[0]
            .data_only_dump_options()
            .expect("one table is flagged");
        assert_eq!(opts.content, DumpContent::DataOnly);
        assert_eq!(
            opts.objects.as_deref(),
            Some(&["sales.lines".to_string()][..])
        );
        assert_eq!(opts.layout, DumpLayout::SingleFile);
    }
}
