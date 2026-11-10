//! Snapshot and delta logic: the immutable [`ModelSnapshot`] (objects plus
//! name/symbol indexes), fragment application producing the next snapshot and
//! a [`ModelDelta`], source-conflict (file-vs-database) resolution, and the
//! snapshot's persistence (serde) format.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use crate::sqlmodel::dialect::{CatalogName, NamespaceName, ObjectKind, ObjectRef};
use crate::sqlmodel::schema::{
    Generation, IncompleteRegions, ObjectEntry, ObjectFragment, ObjectId, Provenance,
    SourceDescriptor,
};

// ---------------------------------------------------------------------------
// Snapshot
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct ModelSnapshot {
    pub generation: Generation,
    objects: HashMap<ObjectId, Arc<ObjectEntry>>,
    /// fold key → ids (multiple kinds may share a name).
    by_ref: HashMap<String, Vec<ObjectId>>,
    /// referenced fold key → ids of objects whose definitions mention it.
    referenced_by: HashMap<String, BTreeSet<ObjectId>>,
    /// bare object-name fold → ids, for unqualified resolution without
    /// scanning every key. Rebuilt on load, never serialized (keeps the
    /// data-model cache format unchanged).
    by_bare_name: HashMap<String, Vec<ObjectId>>,
    pub sources: Vec<SourceDescriptor>,
    pub incomplete: IncompleteRegions,
    next_id: ObjectId,
}

/// Serialized form of [`ModelSnapshot`]: entries flattened out of their `Arc`s
/// (the map key is each entry's own `id`), the two name indexes carried
/// verbatim. Exists so a snapshot can be persisted to a local data-model cache
/// and restored without re-querying the server.
#[derive(Serialize, Deserialize)]
struct ModelSnapshotSerde {
    generation: Generation,
    objects: Vec<ObjectEntry>,
    by_ref: HashMap<String, Vec<ObjectId>>,
    referenced_by: HashMap<String, BTreeSet<ObjectId>>,
    sources: Vec<SourceDescriptor>,
    incomplete: IncompleteRegions,
    next_id: ObjectId,
}

impl Serialize for ModelSnapshot {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut objects: Vec<ObjectEntry> =
            self.objects.values().map(|e| e.as_ref().clone()).collect();
        // Deterministic output: HashMap iteration order is arbitrary.
        objects.sort_by_key(|e| e.id);
        ModelSnapshotSerde {
            generation: self.generation,
            objects,
            by_ref: self.by_ref.clone(),
            referenced_by: self.referenced_by.clone(),
            sources: self.sources.clone(),
            incomplete: self.incomplete.clone(),
            next_id: self.next_id,
        }
        .serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ModelSnapshot {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = ModelSnapshotSerde::deserialize(deserializer)?;
        // Serialized objects are sorted by id (≈ insertion order), so the
        // rebuilt bare-name index lists candidates deterministically.
        let mut by_bare_name: HashMap<String, Vec<ObjectId>> = HashMap::new();
        for e in &raw.objects {
            by_bare_name
                .entry(e.reference.name.fold_key().to_string())
                .or_default()
                .push(e.id);
        }
        Ok(ModelSnapshot {
            generation: raw.generation,
            objects: raw
                .objects
                .into_iter()
                .map(|e| (e.id, Arc::new(e)))
                .collect(),
            by_ref: raw.by_ref,
            referenced_by: raw.referenced_by,
            by_bare_name,
            sources: raw.sources,
            incomplete: raw.incomplete,
            next_id: raw.next_id,
        })
    }
}

impl ModelSnapshot {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.objects.len()
    }

    pub fn is_empty(&self) -> bool {
        self.objects.is_empty()
    }

    pub fn get(&self, id: ObjectId) -> Option<&Arc<ObjectEntry>> {
        self.objects.get(&id)
    }

    pub fn objects(&self) -> impl Iterator<Item = &Arc<ObjectEntry>> {
        self.objects.values()
    }

    /// Exact-reference lookup (all kinds at that name).
    pub fn lookup(&self, reference: &ObjectRef) -> Vec<&Arc<ObjectEntry>> {
        self.by_ref
            .get(&reference.fold_key())
            .map(|ids| ids.iter().filter_map(|id| self.objects.get(id)).collect())
            .unwrap_or_default()
    }

    /// Resolution honoring defaults: tries the reference as written, then with
    /// the default namespace, then with default catalog + namespace, then a
    /// bare-name scan. Mirrors per-dialect search-path behavior closely enough
    /// for validation and completion.
    pub fn resolve(
        &self,
        reference: &ObjectRef,
        default_catalog: Option<&CatalogName>,
        default_namespace: Option<&NamespaceName>,
    ) -> Vec<&Arc<ObjectEntry>> {
        let exact = self.lookup(reference);
        if !exact.is_empty() {
            return exact;
        }
        if reference.catalog.is_none() {
            let mut attempt = reference.clone();
            if attempt.namespace.is_none() {
                attempt.namespace = default_namespace.cloned();
            }
            attempt.catalog = default_catalog.cloned();
            let found = self.lookup(&attempt);
            if !found.is_empty() {
                return found;
            }
            // Bare-name fallback: match by object name alone via the bare
            // index (deterministic candidate order, no full-index scan).
            if reference.namespace.is_none() {
                let bare = reference.name.fold_key();
                if !bare.contains('.') {
                    return self
                        .by_bare_name
                        .get(bare)
                        .map(|ids| ids.iter().filter_map(|id| self.objects.get(id)).collect())
                        .unwrap_or_default();
                }
                // A dotted quoted name (`[foo.bar]`) keeps the legacy suffix
                // scan so it can still match `namespace.name` spellings.
                let suffix = format!(".{bare}");
                let mut out: Vec<&Arc<ObjectEntry>> = Vec::new();
                for (key, ids) in &self.by_ref {
                    if key.ends_with(&suffix) || key == bare {
                        out.extend(ids.iter().filter_map(|id| self.objects.get(id)));
                    }
                }
                return out;
            }
        }
        Vec::new()
    }

    pub fn resolve_kind(
        &self,
        reference: &ObjectRef,
        kind: ObjectKind,
        default_catalog: Option<&CatalogName>,
        default_namespace: Option<&NamespaceName>,
    ) -> Option<&Arc<ObjectEntry>> {
        self.resolve(reference, default_catalog, default_namespace)
            .into_iter()
            .find(|e| e.kind == kind)
    }

    /// Ids of objects whose definitions reference `changed` — the invalidation
    /// fan-out for a delta.
    pub fn dependents_of(&self, changed: &ObjectRef) -> Vec<ObjectId> {
        self.referenced_by
            .get(&changed.fold_key())
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default()
    }

    /// Apply fragments, producing the next snapshot. `remove` lists fold keys
    /// (with provenance filter) whose previous entries should drop — e.g. all
    /// fragments previously sourced from a deleted file.
    pub fn apply(
        &self,
        generation: Generation,
        fragments: Vec<ObjectFragment>,
        remove_file_paths: &BTreeSet<String>,
        incomplete: IncompleteRegions,
    ) -> (ModelSnapshot, ModelDelta) {
        let mut next = self.next_generation(generation, incomplete);
        let mut delta = ModelDelta {
            generation,
            ..ModelDelta::default()
        };
        next.remove_file_entries(remove_file_paths, &mut delta);
        for fragment in fragments {
            next.upsert(fragment, &mut delta);
        }
        coalesce_readds(&mut delta);
        (next, delta)
    }

    /// Start the next snapshot: same contents and indexes, new generation and
    /// incomplete-region set.
    fn next_generation(&self, generation: Generation, incomplete: IncompleteRegions) -> Self {
        ModelSnapshot {
            generation,
            objects: self.objects.clone(),
            by_ref: self.by_ref.clone(),
            referenced_by: self.referenced_by.clone(),
            by_bare_name: self.by_bare_name.clone(),
            sources: self.sources.clone(),
            incomplete,
            next_id: self.next_id,
        }
    }

    /// Drop every entry whose provenance is one of the removed files,
    /// recording each drop in the delta.
    fn remove_file_entries(
        &mut self,
        remove_file_paths: &BTreeSet<String>,
        delta: &mut ModelDelta,
    ) {
        if remove_file_paths.is_empty() {
            return;
        }
        let doomed: Vec<ObjectId> = self
            .objects
            .iter()
            .filter(|(_, e)| match &e.provenance {
                Provenance::File { path, .. } => remove_file_paths.contains(path),
                _ => false,
            })
            .map(|(id, _)| *id)
            .collect();
        for id in doomed {
            if let Some(entry) = self.remove_entry(id) {
                delta.removed.push(entry.reference.clone());
            }
        }
    }

    fn remove_entry(&mut self, id: ObjectId) -> Option<Arc<ObjectEntry>> {
        let entry = self.objects.remove(&id)?;
        let key = entry.reference.fold_key();
        if let Some(ids) = self.by_ref.get_mut(&key) {
            ids.retain(|i| *i != id);
            if ids.is_empty() {
                self.by_ref.remove(&key);
            }
        }
        let bare = entry.reference.name.fold_key();
        if let Some(ids) = self.by_bare_name.get_mut(bare) {
            ids.retain(|i| *i != id);
            if ids.is_empty() {
                self.by_bare_name.remove(bare);
            }
        }
        for set in self.referenced_by.values_mut() {
            set.remove(&id);
        }
        Some(entry)
    }

    fn upsert(&mut self, fragment: ObjectFragment, delta: &mut ModelDelta) {
        let key = fragment.reference.fold_key();
        match self.find_existing(&key, fragment.kind) {
            Some(id) => self.replace_entry(id, fragment, delta),
            None => self.insert_new(key, fragment, delta),
        }
    }

    /// Id of the existing entry at `key` with the same kind, if any.
    fn find_existing(&self, key: &str, kind: ObjectKind) -> Option<ObjectId> {
        self.by_ref.get(key).and_then(|ids| {
            ids.iter()
                .find(|id| {
                    self.objects
                        .get(id)
                        .map(|e| e.kind == kind)
                        .unwrap_or(false)
                })
                .copied()
        })
    }

    /// Merge `fragment` into the existing entry `id`, recording change and
    /// conflict in the delta.
    fn replace_entry(&mut self, id: ObjectId, fragment: ObjectFragment, delta: &mut ModelDelta) {
        let prior = self.objects.get(&id).cloned().expect("indexed id present");
        let (winner, conflicted) = merge_with_prior(&prior, &fragment, id);
        let changed = winner != *prior;
        self.objects.insert(id, Arc::new(winner));
        self.index_references(id, &fragment.references);
        if changed {
            delta.changed.push(fragment.reference.clone());
        }
        if conflicted {
            delta.conflicts.push(SourceConflict {
                reference: fragment.reference,
                kind: fragment.kind,
            });
        }
    }

    /// Insert a fragment with no existing entry at its reference/kind.
    fn insert_new(&mut self, key: String, fragment: ObjectFragment, delta: &mut ModelDelta) {
        let id = self.next_id;
        self.next_id += 1;
        self.objects.insert(
            id,
            Arc::new(ObjectEntry {
                id,
                reference: fragment.reference.clone(),
                kind: fragment.kind,
                dialect: fragment.dialect,
                body: fragment.body,
                provenance: fragment.provenance,
                shadow: None,
                fidelity: fragment.fidelity,
            }),
        );
        self.by_ref.entry(key).or_default().push(id);
        self.by_bare_name
            .entry(fragment.reference.name.fold_key().to_string())
            .or_default()
            .push(id);
        self.index_references(id, &fragment.references);
        delta.added.push(fragment.reference);
    }

    fn index_references(&mut self, id: ObjectId, references: &[ObjectRef]) {
        for set in self.referenced_by.values_mut() {
            set.remove(&id);
        }
        for r in references {
            self.referenced_by
                .entry(r.fold_key())
                .or_default()
                .insert(id);
            // Also index by bare name so unqualified references invalidate.
            self.referenced_by
                .entry(format!("..{}", r.name.fold_key()))
                .or_default()
                .insert(id);
        }
    }

    /// All current file-vs-database conflicts (entries carrying a shadow).
    pub fn drift(&self) -> Vec<SourceConflict> {
        let mut out: Vec<SourceConflict> = self
            .objects
            .values()
            .filter(|e| e.shadow.is_some())
            .map(|e| SourceConflict {
                reference: e.reference.clone(),
                kind: e.kind,
            })
            .collect();
        out.sort_by(|a, b| a.reference.cmp(&b.reference));
        out
    }
}

/// Build an [`ObjectEntry`] carrying `fragment`'s definition under `id`.
fn entry_from_fragment(id: ObjectId, fragment: &ObjectFragment) -> ObjectEntry {
    ObjectEntry {
        id,
        reference: fragment.reference.clone(),
        kind: fragment.kind,
        dialect: fragment.dialect,
        body: fragment.body.clone(),
        provenance: fragment.provenance.clone(),
        shadow: None,
        fidelity: fragment.fidelity.clone(),
    }
}

/// Conflict rule for an incoming fragment against the existing entry:
/// file wins over database; the loser becomes the shadow when bodies differ.
/// Returns the winning entry and whether a source conflict occurred.
fn merge_with_prior(
    prior: &ObjectEntry,
    fragment: &ObjectFragment,
    id: ObjectId,
) -> (ObjectEntry, bool) {
    match (fragment.provenance.is_file(), prior.provenance.is_file()) {
        // Incoming database fragment vs existing file entry:
        // the file keeps winning; the db version becomes shadow.
        (false, true) => {
            let mut kept = prior.clone();
            let db_entry = entry_from_fragment(id, fragment);
            let differs = db_entry.body != kept.body;
            kept.shadow = if differs {
                Some(Box::new(db_entry))
            } else {
                None
            };
            (kept, differs)
        }
        // Incoming file fragment: it wins; a prior database entry (or prior
        // shadow) becomes the shadow.
        (true, false) => {
            let differs = fragment.body != prior.body;
            let mut winner = entry_from_fragment(id, fragment);
            if differs {
                winner.shadow = Some(Box::new(prior.clone()));
            }
            (winner, differs)
        }
        // Same source kind: plain replacement, keep any shadow.
        _ => {
            let mut winner = entry_from_fragment(id, fragment);
            winner.shadow = prior.shadow.clone();
            (winner, false)
        }
    }
}

/// An object removed and re-added in the same generation (a changed file
/// re-collecting) is a change, not a remove+add.
fn coalesce_readds(delta: &mut ModelDelta) {
    let readded: Vec<ObjectRef> = delta
        .removed
        .iter()
        .filter(|r| delta.added.contains(r))
        .cloned()
        .collect();
    for r in readded {
        delta.removed.retain(|x| x != &r);
        delta.added.retain(|x| x != &r);
        if !delta.changed.contains(&r) {
            delta.changed.push(r);
        }
    }
}

// ---------------------------------------------------------------------------
// Delta
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDelta {
    pub generation: Generation,
    pub added: Vec<ObjectRef>,
    pub changed: Vec<ObjectRef>,
    pub removed: Vec<ObjectRef>,
    pub conflicts: Vec<SourceConflict>,
}

impl ModelDelta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.changed.is_empty()
            && self.removed.is_empty()
            && self.conflicts.is_empty()
    }

    pub fn touched(&self) -> impl Iterator<Item = &ObjectRef> {
        self.added
            .iter()
            .chain(self.changed.iter())
            .chain(self.removed.iter())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceConflict {
    pub reference: ObjectRef,
    pub kind: ObjectKind,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sqlmodel::dialect::{Dialect, SqlName};
    use crate::sqlmodel::schema::{ColumnDef, Fidelity, ObjectBody, Span, TableBody};
    use crate::sqlmodel::types::TypeRef;

    fn name(s: &str, d: Dialect) -> SqlName {
        SqlName::new(s, d, false)
    }

    fn table_fragment(
        d: Dialect,
        n: &str,
        provenance: Provenance,
        cols: &[&str],
    ) -> ObjectFragment {
        ObjectFragment {
            reference: ObjectRef::new(None, Some(name("dbo", d)), name(n, d)),
            kind: ObjectKind::Table,
            dialect: d,
            body: ObjectBody::Table(TableBody {
                columns: cols
                    .iter()
                    .enumerate()
                    .map(|(i, c)| ColumnDef {
                        name: name(c, d),
                        ty: Some(TypeRef::parse("int", d)),
                        nullable: Some(true),
                        default: None,
                        identity: false,
                        generated: None,
                        collation: None,
                        ordinal: i,
                    })
                    .collect(),
                ..TableBody::default()
            }),
            provenance,
            fidelity: Fidelity::Exact,
            references: vec![],
        }
    }

    /// The persistent IntelliSense data-model cache serializes snapshots as
    /// JSON across app versions. Function subtype kinds (added after the cache
    /// shipped) must round-trip — and a cache written by a NEWER build with a
    /// kind this build doesn't know must fail deserialization cleanly, which
    /// the cache layer treats as a miss instead of an error.
    #[test]
    fn snapshot_json_round_trips_function_subtypes_and_rejects_unknown_kinds() {
        use crate::sqlmodel::schema::RoutineBody;
        let d = Dialect::Postgres;
        let db = Provenance::Database {
            profile: "p1".into(),
        };
        let function = |n: &str, kind: ObjectKind| ObjectFragment {
            reference: ObjectRef::new(None, Some(name("public", d)), name(n, d)),
            kind,
            dialect: d,
            body: ObjectBody::Routine(RoutineBody {
                params: vec![],
                returns: None,
                language: Some("sql".into()),
                body_text: None,
            }),
            provenance: db.clone(),
            fidelity: Fidelity::Exact,
            references: vec![],
        };
        let (snap, _) = ModelSnapshot::empty().apply(
            1,
            vec![
                function("scale_it", ObjectKind::ScalarFunction),
                function("rows_of", ObjectKind::TableFunction),
                function("agg_it", ObjectKind::AggregateFunction),
            ],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );

        let json = serde_json::to_string(&snap).expect("snapshot serializes");
        let loaded: ModelSnapshot = serde_json::from_str(&json).expect("snapshot deserializes");
        let mut kinds: Vec<ObjectKind> = loaded.objects().map(|o| o.kind).collect();
        kinds.sort();
        assert_eq!(
            kinds,
            vec![
                ObjectKind::ScalarFunction,
                ObjectKind::TableFunction,
                ObjectKind::AggregateFunction,
            ]
        );
        // The bare-name index is rebuilt on load, not persisted; unqualified
        // resolution must work on the deserialized snapshot.
        let bare = ObjectRef::bare(name("rows_of", d));
        assert_eq!(loaded.resolve(&bare, None, None).len(), 1);

        // A kind from a future build must be a parse error (cache miss), not
        // a silently mangled entry.
        let future = json.replace("\"TableFunction\"", "\"WindowFunction\"");
        assert!(future != json, "fixture must contain the kind to corrupt");
        assert!(serde_json::from_str::<ModelSnapshot>(&future).is_err());
    }

    #[test]
    fn snapshot_apply_add_change_remove() {
        let d = Dialect::SqlServer;
        let snap = ModelSnapshot::empty();
        let db = Provenance::Database {
            profile: "p1".into(),
        };

        let (snap, delta) = snap.apply(
            1,
            vec![table_fragment(d, "Person", db.clone(), &["Id", "Name"])],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        assert_eq!(delta.added.len(), 1);
        assert_eq!(snap.len(), 1);

        let r = ObjectRef::new(None, Some(name("dbo", d)), name("PERSON", d));
        assert_eq!(snap.lookup(&r).len(), 1);

        // Change via file → conflict + shadow (file wins).
        let file = Provenance::File {
            path: "person.sql".into(),
            content_hash: 7,
            span: Span::new(0, 10),
        };
        let (snap, delta) = snap.apply(
            2,
            vec![table_fragment(d, "Person", file, &["Id", "Name", "Email"])],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        assert_eq!(delta.changed.len(), 1);
        assert_eq!(delta.conflicts.len(), 1);
        let entry = snap.lookup(&r)[0];
        assert!(entry.provenance.is_file());
        assert!(entry.shadow.is_some());
        assert_eq!(entry.table().unwrap().columns.len(), 3);
        assert_eq!(
            entry
                .shadow
                .as_ref()
                .unwrap()
                .table()
                .unwrap()
                .columns
                .len(),
            2
        );
        assert_eq!(snap.drift().len(), 1);

        // Removing the file drops the entry.
        let mut removed = BTreeSet::new();
        removed.insert("person.sql".to_string());
        let (snap, delta) = snap.apply(3, vec![], &removed, IncompleteRegions::default());
        assert_eq!(delta.removed.len(), 1);
        assert!(snap.lookup(&r).is_empty());
    }

    #[test]
    fn db_fragment_does_not_displace_file_entry() {
        let d = Dialect::SqlServer;
        let file = Provenance::File {
            path: "t.sql".into(),
            content_hash: 1,
            span: Span::new(0, 5),
        };
        let (snap, _) = ModelSnapshot::empty().apply(
            1,
            vec![table_fragment(d, "T", file, &["A", "B"])],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let (snap, delta) = snap.apply(
            2,
            vec![table_fragment(d, "T", db, &["A"])],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        let r = ObjectRef::new(None, Some(name("dbo", d)), name("t", d));
        let entry = snap.lookup(&r)[0];
        assert!(
            entry.provenance.is_file(),
            "file definition must keep winning"
        );
        assert_eq!(entry.table().unwrap().columns.len(), 2);
        assert!(entry.shadow.is_some());
        assert_eq!(delta.conflicts.len(), 1);
    }

    #[test]
    fn dependents_invalidation_index() {
        let d = Dialect::SqlServer;
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let mut frag_view = table_fragment(d, "V", db.clone(), &["X"]);
        frag_view.kind = ObjectKind::View;
        frag_view.references = vec![ObjectRef::new(
            None,
            Some(name("dbo", d)),
            name("Person", d),
        )];

        let (snap, _) = ModelSnapshot::empty().apply(
            1,
            vec![table_fragment(d, "Person", db, &["Id"]), frag_view],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        let person = ObjectRef::new(None, Some(name("dbo", d)), name("person", d));
        let deps = snap.dependents_of(&person);
        assert_eq!(deps.len(), 1);
        assert_eq!(snap.get(deps[0]).unwrap().reference.name.spelling(), "V");
    }

    #[test]
    fn resolve_with_defaults_and_bare_name() {
        let d = Dialect::SqlServer;
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let (snap, _) = ModelSnapshot::empty().apply(
            1,
            vec![table_fragment(d, "Person", db, &["Id"])],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );
        let bare = ObjectRef::bare(name("person", d));
        let dbo = name("dbo", d);
        assert_eq!(snap.resolve(&bare, None, Some(&dbo)).len(), 1);
        assert_eq!(
            snap.resolve(&bare, None, None).len(),
            1,
            "bare-name fallback"
        );
        let wrong = ObjectRef::new(None, Some(name("sales", d)), name("person", d));
        assert!(snap.resolve(&wrong, None, None).is_empty());
    }

    /// A snapshot survives a serde round-trip (the persistent data-model cache
    /// path): objects, name index, and lookups all come back intact.
    #[test]
    fn snapshot_serde_roundtrip_preserves_objects_and_index() {
        let d = Dialect::SqlServer;
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let (snap, _) = ModelSnapshot::empty().apply(
            3,
            vec![
                table_fragment(d, "Orders", db.clone(), &["Id", "Total"]),
                table_fragment(d, "Customers", db, &["Id", "Name"]),
            ],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );

        let json = serde_json::to_string(&snap).expect("serialize");
        let restored: ModelSnapshot = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(restored.generation, snap.generation);
        assert_eq!(restored.len(), snap.len());
        let orders = ObjectRef::new(None, Some(name("dbo", d)), name("orders", d));
        let hits = restored.lookup(&orders);
        assert_eq!(hits.len(), 1);
        let cols = &hits[0].table().expect("table body").columns;
        assert_eq!(cols.len(), 2);
        assert_eq!(cols[1].name.spelling(), "Total");
    }

    /// Bare-name resolution goes through the dedicated index: unqualified
    /// lookups find objects across namespaces (ambiguity preserved), and the
    /// index survives a serde roundtrip (it is rebuilt, not serialized).
    #[test]
    fn bare_name_resolution_uses_rebuilt_index() {
        let d = Dialect::SqlServer;
        let mut sales = table_fragment(
            d,
            "Orders",
            Provenance::Database {
                profile: "p".into(),
            },
            &["Id"],
        );
        sales.reference = ObjectRef::new(None, Some(name("sales", d)), name("Orders", d));
        let mut archive = table_fragment(
            d,
            "Orders",
            Provenance::Database {
                profile: "p".into(),
            },
            &["Id"],
        );
        archive.reference = ObjectRef::new(None, Some(name("archive", d)), name("Orders", d));
        let (snapshot, _) = ModelSnapshot::empty().apply(
            1,
            vec![sales, archive],
            &BTreeSet::new(),
            IncompleteRegions::default(),
        );

        let bare = ObjectRef::new(None, None, name("Orders", d));
        let hits = snapshot.resolve(&bare, None, None);
        assert_eq!(hits.len(), 2, "both namespaces must surface as candidates");

        let json = serde_json::to_string(&snapshot).unwrap();
        assert!(
            !json.contains("by_bare_name"),
            "index must not be serialized"
        );
        let restored: ModelSnapshot = serde_json::from_str(&json).unwrap();
        let hits = restored.resolve(&bare, None, None);
        assert_eq!(hits.len(), 2, "index must be rebuilt on load");
    }

    // -- focused tests for apply()'s pure helpers ---------------------------

    fn entry_of(fragment: &ObjectFragment, id: ObjectId) -> ObjectEntry {
        super::entry_from_fragment(id, fragment)
    }

    #[test]
    fn merge_with_prior_identical_bodies_no_conflict() {
        let d = Dialect::SqlServer;
        let file = Provenance::File {
            path: "t.sql".into(),
            content_hash: 1,
            span: Span::new(0, 5),
        };
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let prior = entry_of(&table_fragment(d, "T", file, &["A"]), 7);
        let incoming = table_fragment(d, "T", db, &["A"]);
        // Same body from the database: file keeps winning, no shadow, no
        // conflict, and the entry is unchanged.
        let (winner, conflicted) = super::merge_with_prior(&prior, &incoming, 7);
        assert!(!conflicted);
        assert!(winner.shadow.is_none());
        assert_eq!(winner, prior);
    }

    #[test]
    fn merge_with_prior_same_source_keeps_shadow() {
        let d = Dialect::SqlServer;
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let mut prior = entry_of(&table_fragment(d, "T", db.clone(), &["A"]), 3);
        prior.shadow = Some(Box::new(entry_of(
            &table_fragment(d, "T", db.clone(), &["Old"]),
            3,
        )));
        let incoming = table_fragment(d, "T", db, &["A", "B"]);
        // Same source kind replaces the body but carries the shadow forward
        // and reports no new conflict.
        let (winner, conflicted) = super::merge_with_prior(&prior, &incoming, 3);
        assert!(!conflicted);
        assert!(winner.shadow.is_some());
        assert_eq!(winner.table().unwrap().columns.len(), 2);
    }

    #[test]
    fn merge_with_prior_file_wins_and_shadows_db() {
        let d = Dialect::SqlServer;
        let db = Provenance::Database {
            profile: "p".into(),
        };
        let file = Provenance::File {
            path: "t.sql".into(),
            content_hash: 9,
            span: Span::new(0, 4),
        };
        let prior = entry_of(&table_fragment(d, "T", db, &["A"]), 5);
        let incoming = table_fragment(d, "T", file, &["A", "B"]);
        let (winner, conflicted) = super::merge_with_prior(&prior, &incoming, 5);
        assert!(conflicted);
        assert!(winner.provenance.is_file());
        let shadow = winner.shadow.as_ref().expect("db entry becomes shadow");
        assert_eq!(shadow.table().unwrap().columns.len(), 1);
    }

    #[test]
    fn coalesce_readds_folds_remove_add_into_change() {
        let d = Dialect::SqlServer;
        let person = ObjectRef::new(None, Some(name("dbo", d)), name("Person", d));
        let other = ObjectRef::new(None, Some(name("dbo", d)), name("Other", d));
        let mut delta = ModelDelta {
            generation: 2,
            added: vec![person.clone()],
            changed: vec![],
            removed: vec![person.clone(), other.clone()],
            conflicts: vec![],
        };
        super::coalesce_readds(&mut delta);
        assert!(delta.added.is_empty());
        assert_eq!(delta.removed, vec![other]);
        assert_eq!(delta.changed, vec![person]);

        // Already-changed refs are not duplicated.
        let r = ObjectRef::new(None, None, name("X", d));
        let mut delta = ModelDelta {
            generation: 1,
            added: vec![r.clone()],
            changed: vec![r.clone()],
            removed: vec![r.clone()],
            conflicts: vec![],
        };
        super::coalesce_readds(&mut delta);
        assert_eq!(delta.changed, vec![r]);
        assert!(delta.added.is_empty() && delta.removed.is_empty());
    }
}
