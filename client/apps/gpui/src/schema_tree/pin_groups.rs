//! Named groups of pinned explorer objects ("one set of pins per task").
//!
//! A group belongs to one server and shows as a folder at the top of that
//! server, listing its pins in order. Pins carry an optional group id; `None`
//! is the ungrouped default, which is how every pin saved before groups
//! existed loads, and those keep floating to the top of their own folder.
//! Group definitions persist in `explorer-pin-groups.json` beside the pin
//! list (a separate file, so the pin list keeps the plain-array shape older
//! builds read).

use std::collections::HashMap;

use uuid::Uuid;

use super::load::LoadState;
use super::model::{
    FAVORITES_CAP, FavoriteObject, NodeKind, PinnedKind, TreeNode, move_pinned, pinned_for_server,
    renumber_pins, same_pin,
};

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PinGroup {
    pub id: Uuid,
    pub server_id: Uuid,
    pub name: String,
    #[serde(default)]
    pub order: u32,
}

pub const PIN_GROUPS_PER_SERVER_CAP: usize = 50;

const PIN_GROUPS_FILE: &str = "explorer-pin-groups.json";

/// Why a group name was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PinGroupNameError {
    Empty,
    /// Another group on the same server has this name (ignoring case).
    Taken,
    TooMany,
}

impl PinGroupNameError {
    pub fn message(self) -> String {
        crate::localization::text(match self {
            Self::Empty => "explorer.schema_tree.pin_group_name_required",
            Self::Taken => "explorer.schema_tree.pin_group_name_taken",
            Self::TooMany => "explorer.schema_tree.pin_group_limit",
        })
    }
}

/// A server's groups in display order.
pub fn groups_for_server(groups: &[PinGroup], server_id: Uuid) -> Vec<&PinGroup> {
    let mut list: Vec<&PinGroup> = groups.iter().filter(|g| g.server_id == server_id).collect();
    list.sort_by_key(|g| g.order);
    list
}

fn check_name<'a>(
    groups: &[PinGroup],
    server_id: Uuid,
    name: &'a str,
    except: Option<Uuid>,
) -> Result<&'a str, PinGroupNameError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(PinGroupNameError::Empty);
    }
    let lower = name.to_lowercase();
    if groups
        .iter()
        .any(|g| g.server_id == server_id && Some(g.id) != except && g.name.to_lowercase() == lower)
    {
        return Err(PinGroupNameError::Taken);
    }
    Ok(name)
}

/// A new group for `server_id`, ordered after its existing groups. Pure: the
/// caller pushes it.
pub fn create_pin_group(
    groups: &[PinGroup],
    server_id: Uuid,
    name: &str,
    id: Uuid,
) -> Result<PinGroup, PinGroupNameError> {
    let existing = groups_for_server(groups, server_id);
    if existing.len() >= PIN_GROUPS_PER_SERVER_CAP {
        return Err(PinGroupNameError::TooMany);
    }
    let name = check_name(groups, server_id, name, None)?;
    let order = existing.last().map_or(0, |g| g.order + 1);
    Ok(PinGroup {
        id,
        server_id,
        name: name.to_string(),
        order,
    })
}

pub fn rename_pin_group(
    groups: &mut [PinGroup],
    id: Uuid,
    name: &str,
) -> Result<(), PinGroupNameError> {
    let Some(server_id) = groups.iter().find(|g| g.id == id).map(|g| g.server_id) else {
        return Ok(());
    };
    let name = check_name(groups, server_id, name, Some(id))?.to_string();
    if let Some(group) = groups.iter_mut().find(|g| g.id == id) {
        group.name = name;
    }
    Ok(())
}

/// Remove a group; its objects stay pinned, back in the ungrouped default.
pub fn delete_pin_group(groups: &mut Vec<PinGroup>, favorites: &mut [FavoriteObject], id: Uuid) {
    groups.retain(|g| g.id != id);
    for favorite in favorites.iter_mut().filter(|f| f.group == Some(id)) {
        favorite.group = None;
    }
}

/// Put `pin` in `group` (`None` = ungrouped), as the group's last entry.
/// An object that is not pinned yet is pinned; at the pin cap the oldest
/// pin makes room rather than the new one.
pub fn assign_pin_group(
    mut favorites: Vec<FavoriteObject>,
    pin: &FavoriteObject,
    group: Option<Uuid>,
) -> Vec<FavoriteObject> {
    let mut moved = match favorites.iter().position(|f| same_pin(f, pin)) {
        Some(at) => favorites.remove(at),
        None => {
            if favorites.len() >= FAVORITES_CAP {
                favorites.truncate(FAVORITES_CAP - 1);
            }
            pin.clone()
        }
    };
    moved.group = group;
    favorites.push(moved);
    renumber_pins(&mut favorites);
    favorites
}

/// One server's pins in `group`, in order, each with its index into
/// [`pinned_for_server`] (the index `NodeKind::Pinned` carries).
pub fn pins_in_group(
    favorites: &[FavoriteObject],
    server_id: Uuid,
    group: Option<Uuid>,
) -> Vec<(usize, &FavoriteObject)> {
    pinned_for_server(favorites, server_id)
        .into_iter()
        .enumerate()
        .filter(|(_, f)| f.group == group)
        .collect()
}

/// Move the pin at `index` (into [`pinned_for_server`]) one step up
/// (`delta < 0`) or down within its own group, past any pins of other groups
/// in between. Unchanged at either end of the group.
pub fn move_pinned_in_group(
    favorites: Vec<FavoriteObject>,
    server_id: Uuid,
    index: usize,
    delta: isize,
) -> Vec<FavoriteObject> {
    let server_pins = pinned_for_server(&favorites, server_id);
    let Some(current) = server_pins.get(index) else {
        return favorites;
    };
    let group = current.group;
    let same_group: Vec<usize> = server_pins
        .iter()
        .enumerate()
        .filter(|(_, f)| f.group == group)
        .map(|(i, _)| i)
        .collect();
    let Some(at) = same_group.iter().position(|&i| i == index) else {
        return favorites;
    };
    let target = at as isize + delta.signum();
    if target < 0 || target as usize >= same_group.len() {
        return favorites;
    }
    let neighbor = same_group[target as usize];
    let vec_index = |list: &[FavoriteObject], pinned: &FavoriteObject| {
        list.iter().position(|f| same_pin(f, pinned))
    };
    let (Some(from), Some(to)) = (
        vec_index(&favorites, server_pins[index]),
        vec_index(&favorites, server_pins[neighbor]),
    ) else {
        return favorites;
    };
    move_pinned(favorites, from, to)
}

/// Pins whose group no longer exists fall back to the ungrouped default.
pub fn drop_unknown_groups(favorites: &mut [FavoriteObject], groups: &[PinGroup]) {
    for favorite in favorites.iter_mut() {
        if let Some(id) = favorite.group
            && !groups
                .iter()
                .any(|g| g.id == id && g.server_id == favorite.server_id)
        {
            favorite.group = None;
        }
    }
}

/// Row text for a pinned object in a group folder.
fn pinned_title(favorite: &FavoriteObject) -> String {
    let qualified = match (&favorite.schema, favorite.kind) {
        (_, PinnedKind::Database) | (None, _) => favorite.name.clone(),
        (Some(schema), _) => format!("{schema}.{}", favorite.name),
    };
    match favorite
        .database
        .as_deref()
        .filter(|_| favorite.kind != PinnedKind::Database)
    {
        Some(database) => crate::localization::format(
            "explorer.schema_tree.pinned_in_database",
            &[("name", &qualified), ("database", &database.to_string())],
        ),
        None => qualified,
    }
}

fn folder_id(server_node_id: &str, group: Uuid) -> String {
    format!("{server_node_id}:pingroup:{group}")
}

/// Rebuild one server node's group folders at the front of its children,
/// replacing any from a previous pass and keeping each folder's expansion.
pub(crate) fn apply_pin_group_folders_to_server(
    server: &mut TreeNode,
    favorites: &[FavoriteObject],
    groups: &[PinGroup],
) {
    let NodeKind::Server { id: server_id } = server.kind else {
        return;
    };
    let mut expanded: HashMap<String, bool> = HashMap::new();
    server.children.retain(|child| {
        if matches!(child.kind, NodeKind::PinnedFolder { .. }) {
            expanded.insert(child.id.clone(), child.expanded);
            false
        } else {
            true
        }
    });
    let depth = server.depth + 1;
    let folders: Vec<TreeNode> = groups_for_server(groups, server_id)
        .into_iter()
        .map(|group| {
            let id = folder_id(&server.id, group.id);
            let mut folder = TreeNode::new(
                id.clone(),
                group.name.clone(),
                NodeKind::PinnedFolder {
                    server_id,
                    group: group.id,
                },
                depth,
            );
            folder.expanded = expanded.get(&id).copied().unwrap_or(true);
            folder.load_state = LoadState::Loaded;
            folder.children = pins_in_group(favorites, server_id, Some(group.id))
                .into_iter()
                .map(|(index, favorite)| {
                    let mut row = TreeNode::new(
                        format!("{id}:pin:{index}"),
                        pinned_title(favorite),
                        NodeKind::Pinned { index },
                        depth + 1,
                    );
                    row.load_state = LoadState::Loaded;
                    row
                })
                .collect();
            folder
        })
        .collect();
    server.children.splice(0..0, folders);
}

/// [`apply_pin_group_folders_to_server`] for every server in the tree,
/// including servers nested under grouping headers.
pub(crate) fn apply_pin_group_folders(
    roots: &mut [TreeNode],
    favorites: &[FavoriteObject],
    groups: &[PinGroup],
) {
    for node in roots.iter_mut() {
        if matches!(node.kind, NodeKind::Server { .. }) {
            apply_pin_group_folders_to_server(node, favorites, groups);
        } else if matches!(node.kind, NodeKind::Group { .. }) {
            apply_pin_group_folders(&mut node.children, favorites, groups);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn load_pin_groups() -> Vec<PinGroup> {
    let mut path = crate::preferences::preferences_path();
    path.push(PIN_GROUPS_FILE);
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn load_pin_groups() -> Vec<PinGroup> {
    crate::host_storage::load_text(crate::host_storage::StorageArea::Data, PIN_GROUPS_FILE)
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn save_pin_groups(groups: &[PinGroup]) {
    let mut path = crate::preferences::preferences_path();
    path.push(PIN_GROUPS_FILE);
    if let Ok(text) = serde_json::to_string_pretty(groups) {
        let _ = std::fs::write(path, text);
    }
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn save_pin_groups(groups: &[PinGroup]) {
    if let Ok(text) = serde_json::to_string_pretty(groups) {
        let _ = crate::host_storage::store_text(
            crate::host_storage::StorageArea::Data,
            PIN_GROUPS_FILE,
            &text,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVER: Uuid = Uuid::from_u128(7);
    const OTHER_SERVER: Uuid = Uuid::from_u128(8);

    fn pin(name: &str) -> FavoriteObject {
        FavoriteObject {
            server_id: SERVER,
            database: Some("db".into()),
            schema: Some("dbo".into()),
            name: name.into(),
            kind: PinnedKind::Table,
            order: 0,
            group: None,
        }
    }

    fn pins(names: &[&str]) -> Vec<FavoriteObject> {
        let mut list: Vec<FavoriteObject> = names.iter().map(|n| pin(n)).collect();
        renumber_pins(&mut list);
        list
    }

    fn names(list: &[(usize, &FavoriteObject)]) -> Vec<String> {
        list.iter().map(|(_, f)| f.name.clone()).collect()
    }

    fn group(groups: &mut Vec<PinGroup>, name: &str, n: u128) -> Uuid {
        let created = create_pin_group(groups, SERVER, name, Uuid::from_u128(100 + n)).unwrap();
        let id = created.id;
        groups.push(created);
        id
    }

    #[test]
    fn create_trims_rejects_blank_and_duplicate_names_per_server() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "  Billing ", 1);
        assert_eq!(groups[0].name, "Billing");
        assert_eq!(groups[0].id, billing);
        assert_eq!(
            create_pin_group(&groups, SERVER, "   ", Uuid::from_u128(2)),
            Err(PinGroupNameError::Empty)
        );
        assert_eq!(
            create_pin_group(&groups, SERVER, "billing", Uuid::from_u128(3)),
            Err(PinGroupNameError::Taken)
        );
        // Another server may reuse the name.
        assert!(create_pin_group(&groups, OTHER_SERVER, "Billing", Uuid::from_u128(4)).is_ok());
        let onboarding = group(&mut groups, "Onboarding", 2);
        let listed: Vec<Uuid> = groups_for_server(&groups, SERVER)
            .iter()
            .map(|g| g.id)
            .collect();
        assert_eq!(listed, vec![billing, onboarding]);
    }

    #[test]
    fn create_stops_at_the_per_server_cap() {
        let mut groups = Vec::new();
        for i in 0..PIN_GROUPS_PER_SERVER_CAP {
            group(&mut groups, &format!("g{i}"), i as u128);
        }
        assert_eq!(
            create_pin_group(&groups, SERVER, "one more", Uuid::from_u128(999)),
            Err(PinGroupNameError::TooMany)
        );
    }

    #[test]
    fn rename_checks_the_new_name_but_allows_its_own() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        group(&mut groups, "Onboarding", 2);
        assert_eq!(
            rename_pin_group(&mut groups, billing, "onboarding"),
            Err(PinGroupNameError::Taken)
        );
        assert_eq!(
            rename_pin_group(&mut groups, billing, ""),
            Err(PinGroupNameError::Empty)
        );
        assert_eq!(rename_pin_group(&mut groups, billing, "BILLING"), Ok(()));
        assert_eq!(groups[0].name, "BILLING");
    }

    #[test]
    fn assign_moves_between_groups_and_pins_unpinned_objects() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        let onboarding = group(&mut groups, "Onboarding", 2);
        let list = pins(&["a", "b", "c"]);
        let list = assign_pin_group(list, &pin("a"), Some(billing));
        let list = assign_pin_group(list, &pin("c"), Some(billing));
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(billing))),
            vec!["a", "c"]
        );
        assert_eq!(names(&pins_in_group(&list, SERVER, None)), vec!["b"]);
        // Move between groups: lands last in the new group.
        let list = assign_pin_group(list, &pin("a"), Some(onboarding));
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(billing))),
            vec!["c"]
        );
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(onboarding))),
            vec!["a"]
        );
        // Back to the ungrouped default.
        let list = assign_pin_group(list, &pin("c"), None);
        assert!(pins_in_group(&list, SERVER, Some(billing)).is_empty());
        // An object that was not pinned yet is pinned straight into the group.
        let list = assign_pin_group(list, &pin("d"), Some(billing));
        assert_eq!(list.len(), 4);
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(billing))),
            vec!["d"]
        );
        // Orders stay a dense 0..n sequence.
        let mut orders: Vec<u32> = list.iter().map(|f| f.order).collect();
        orders.sort();
        assert_eq!(orders, vec![0, 1, 2, 3]);
    }

    #[test]
    fn pins_in_group_reports_indexes_into_the_server_list() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        let mut list = pins(&["a", "b", "c"]);
        list[2].group = Some(billing);
        let in_group = pins_in_group(&list, SERVER, Some(billing));
        assert_eq!(in_group.len(), 1);
        let (index, favorite) = in_group[0];
        assert_eq!(pinned_for_server(&list, SERVER)[index].name, favorite.name);
    }

    #[test]
    fn assign_at_the_cap_drops_the_oldest_pin_not_the_new_one() {
        let mut list = Vec::new();
        for i in 0..FAVORITES_CAP {
            list.push(pin(&format!("t{i}")));
        }
        renumber_pins(&mut list);
        let list = assign_pin_group(list, &pin("new"), Some(Uuid::from_u128(5)));
        assert_eq!(list.len(), FAVORITES_CAP);
        assert!(list.iter().any(|f| f.name == "new"));
    }

    #[test]
    fn move_within_group_skips_pins_of_other_groups() {
        let g = Uuid::from_u128(1);
        let h = Uuid::from_u128(2);
        let mut list = pins(&["a", "x", "b", "y", "c"]);
        for f in list.iter_mut() {
            f.group = Some(if ["x", "y"].contains(&f.name.as_str()) {
                h
            } else {
                g
            });
        }
        // "c" is index 4 in the server list; up once swaps it past "y".
        let list = move_pinned_in_group(list, SERVER, 4, -1);
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(g))),
            vec!["a", "c", "b"]
        );
        assert_eq!(
            names(&pins_in_group(&list, SERVER, Some(h))),
            vec!["x", "y"]
        );
        // At the edge nothing moves.
        let first = pins_in_group(&list, SERVER, Some(g))[0].0;
        let same = move_pinned_in_group(list.clone(), SERVER, first, -1);
        assert_eq!(same, list);
    }

    #[test]
    fn delete_group_keeps_its_objects_pinned_and_ungrouped() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        let mut list = pins(&["a", "b"]);
        list[0].group = Some(billing);
        delete_pin_group(&mut groups, &mut list, billing);
        assert!(groups.is_empty());
        assert_eq!(list.len(), 2);
        assert!(list.iter().all(|f| f.group.is_none()));
    }

    #[test]
    fn pins_naming_a_missing_group_fall_back_to_ungrouped() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        let mut list = pins(&["a", "b"]);
        list[0].group = Some(billing);
        list[1].group = Some(Uuid::from_u128(404));
        drop_unknown_groups(&mut list, &groups);
        assert_eq!(list[0].group, Some(billing));
        assert_eq!(list[1].group, None);
    }

    #[test]
    fn pin_groups_round_trip_through_json() {
        let mut groups = Vec::new();
        group(&mut groups, "Billing", 1);
        let text = serde_json::to_string(&groups).unwrap();
        let back: Vec<PinGroup> = serde_json::from_str(&text).unwrap();
        assert_eq!(back, groups);
    }

    #[test]
    fn group_folders_lead_their_server_and_list_their_pins() {
        let mut groups = Vec::new();
        let billing = group(&mut groups, "Billing", 1);
        let empty = group(&mut groups, "Empty", 2);
        let mut list = pins(&["Orders", "Invoices"]);
        list[1].group = Some(billing);
        let sid = format!("srv:{SERVER}");
        let mut server = TreeNode::new(sid.clone(), "Prod", NodeKind::Server { id: SERVER }, 0);
        server.children = vec![TreeNode::new(
            format!("{sid}:db:db"),
            "db",
            NodeKind::Database("db".into()),
            1,
        )];
        let mut roots = vec![server];
        apply_pin_group_folders(&mut roots, &list, &groups);
        let kids = &roots[0].children;
        assert_eq!(kids.len(), 3, "two group folders, then the database");
        assert!(matches!(
            kids[0].kind,
            NodeKind::PinnedFolder { group, .. } if group == billing
        ));
        assert!(matches!(
            kids[1].kind,
            NodeKind::PinnedFolder { group, .. } if group == empty
        ));
        assert!(matches!(kids[2].kind, NodeKind::Database(_)));
        assert_eq!(kids[0].depth, 1);
        assert_eq!(kids[0].children.len(), 1);
        let pinned = &kids[0].children[0];
        assert!(matches!(pinned.kind, NodeKind::Pinned { index: 1 }));
        assert_eq!(pinned.depth, 2);
        assert!(super::super::model::server_uuid_from_id(&pinned.id) == Some(SERVER));
        assert!(kids[1].children.is_empty());

        // Re-applying replaces the folders (no duplicates) and keeps a
        // collapsed folder collapsed.
        roots[0].children[0].expanded = false;
        apply_pin_group_folders(&mut roots, &list, &groups);
        let kids = &roots[0].children;
        assert_eq!(kids.len(), 3);
        assert!(!kids[0].expanded);
        assert!(kids[1].expanded);

        // Deleting every group removes the folders.
        apply_pin_group_folders(&mut roots, &list, &[]);
        assert_eq!(roots[0].children.len(), 1);
    }

    #[test]
    fn group_folders_reach_servers_under_grouping_headers() {
        let mut groups = Vec::new();
        group(&mut groups, "Billing", 1);
        let sid = format!("srv:{SERVER}");
        let server = TreeNode::new(sid, "Prod", NodeKind::Server { id: SERVER }, 1);
        let mut header = TreeNode::new(
            "group:x",
            "Client",
            NodeKind::Group {
                color_hex: None,
                image_path: None,
            },
            0,
        );
        header.children = vec![server];
        let mut roots = vec![header];
        apply_pin_group_folders(&mut roots, &[], &groups);
        let folder = &roots[0].children[0].children[0];
        assert!(matches!(folder.kind, NodeKind::PinnedFolder { .. }));
        assert_eq!(folder.depth, 2);
    }
}
