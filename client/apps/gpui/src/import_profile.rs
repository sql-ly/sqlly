//! Saved import mapping profiles for the CSV/JSON/Excel import wizard.
//!
//! A profile captures the wizard's full configuration — delimiter, header
//! handling, sheet, null token, column mapping, type overrides, and the
//! create-vs-existing target choice — scoped to one target table identity
//! (engine + connection + database + schema + table). Profiles are listed,
//! applied, renamed, and deleted from the wizard. One default per target
//! auto-applies: when the wizard's target table is (or becomes) the
//! default's table, or when a newly loaded file is recognisably the same feed
//! (same file-name pattern with dates/numbers ignored, or same header columns)
//! as exactly one default saved on this connection and database. Persisted
//! as JSON in the data dir alongside `dump-schedules.json`. Desktop-only (the
//! browser preview cannot read local files, so there is nothing to map).

use std::path::PathBuf;

/// Target identity a profile is scoped to. `engine` is the engine display
/// name (profiles survive a connection's port/host edits but not an engine
/// switch); `connection_id` is the saved connection's stable id.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportScope {
    pub engine: String,
    pub connection_id: uuid::Uuid,
    pub database: String,
    pub schema: String,
    pub table: String,
}

impl ImportScope {
    /// The connection-level part of the key: profiles list per target
    /// database, while the default auto-applies only on an exact match.
    pub fn same_connection(&self, other: &ImportScope) -> bool {
        self.is_saved_connection()
            && self.engine == other.engine
            && self.connection_id == other.connection_id
            && self.database == other.database
    }

    /// Profiles belong to a saved connection. A wizard opened without one
    /// has the nil id, which must never become a bucket shared by every
    /// unsaved connection.
    pub fn is_saved_connection(&self) -> bool {
        !self.connection_id.is_nil()
    }
}

/// A file name with its variable parts folded: lower-cased, and every run of
/// digits (with `-`, `_`, `.` or spaces joining digit groups, as in dates
/// and times) replaced by `#`. `vendor_2026-09-28.csv` and
/// `Vendor_2026-10-05.csv` both give `vendor_#.csv`. `None` when the stem
/// has no letters — a bare date would match every dated file.
pub fn source_pattern(file_name: &str) -> Option<String> {
    let lower = file_name.to_lowercase();
    let chars: Vec<char> = lower.chars().collect();
    let mut out = String::with_capacity(lower.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i].is_ascii_digit() {
            // A digit run, plus separators that join it to more digits.
            while i < chars.len()
                && (chars[i].is_ascii_digit()
                    || (matches!(chars[i], '-' | '_' | '.' | ' ')
                        && chars.get(i + 1).is_some_and(char::is_ascii_digit)))
            {
                i += 1;
            }
            out.push('#');
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    let stem = out.rsplit_once('.').map_or(out.as_str(), |(stem, _)| stem);
    stem.chars().any(char::is_alphabetic).then_some(out)
}

/// The source the wizard currently holds, for default auto-apply matching.
#[derive(Clone, Debug, Default)]
pub struct LoadedSource<'a> {
    /// File name (not path) of a loaded file; `None` for clipboard text.
    pub file_name: Option<&'a str>,
    /// Header/source column names as the wizard reads them.
    pub columns: &'a [String],
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportProfile {
    pub id: uuid::Uuid,
    pub name: String,
    pub scope: ImportScope,
    /// Single-character delimiter as a string (",", ";", "\t", "|").
    pub delimiter: String,
    pub has_header: bool,
    /// Excel sheet name when the profile was saved against a workbook.
    pub sheet: Option<String>,
    pub null_token: String,
    /// Positional mapping spec exactly as the wizard's Column mapping field
    /// holds it (comma-separated target names, `-` skips a source column).
    pub mapping_spec: String,
    /// Positional type-override spec (`auto` keeps the inferred type).
    pub types_spec: String,
    pub create_table: bool,
    pub bulk_insert: bool,
    /// Source column names the two positional specs were saved against —
    /// the baseline for drift rebasing when a later file's columns differ.
    pub source_columns: Vec<String>,
    pub is_default: bool,
    /// [`source_pattern`] of the file the profile was saved from, so a later
    /// dated file of the same feed finds its default. Absent in profiles
    /// saved before this existed and in clipboard-sourced profiles.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_pattern: Option<String>,
}

impl ImportProfile {
    /// Picker label: name plus the target table it is scoped to, so two
    /// profiles with one name on different tables are told apart.
    pub fn label(&self) -> String {
        let target = if self.scope.schema.is_empty() {
            self.scope.table.clone()
        } else {
            format!("{}.{}", self.scope.schema, self.scope.table)
        };
        if self.is_default {
            crate::localization::format(
                "results.import_profiles.default_label",
                &[("profile_name", &self.name), ("table_name", &target)],
            )
        } else {
            crate::localization::format(
                "results.import_profiles.label",
                &[("profile_name", &self.name), ("table_name", &target)],
            )
        }
    }
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub struct ImportProfiles {
    pub profiles: Vec<ImportProfile>,
}

impl ImportProfiles {
    fn path() -> PathBuf {
        crate::preferences::preferences_path().join("import-profiles.json")
    }

    /// Load, distinguishing "no file yet" (empty) from "file unreadable".
    pub fn load() -> Result<Self, String> {
        Self::load_from(&Self::path())
    }

    fn load_from(path: &std::path::Path) -> Result<Self, String> {
        match crate::fs_safe::load_json_store(path) {
            crate::fs_safe::StoreLoad::Missing => Ok(Self::default()),
            crate::fs_safe::StoreLoad::Loaded(store) => Ok(store),
            crate::fs_safe::StoreLoad::Corrupt { error } => {
                Err(crate::localization::message(error))
            }
        }
    }

    /// Atomically persist; refuses to replace a file that exists but does
    /// not parse (the user's profiles are in it).
    pub fn save(&self) -> Result<(), String> {
        self.save_to(&Self::path())
    }

    fn save_to(&self, path: &std::path::Path) -> Result<(), String> {
        if let crate::fs_safe::StoreLoad::Corrupt { error } =
            crate::fs_safe::load_json_store::<Self>(path)
        {
            return Err(crate::localization::format(
                "results.import_profiles.save_blocked",
                &[("error", &crate::localization::message(error))],
            ));
        }
        let text = serde_json::to_string_pretty(self).map_err(|err| err.to_string())?;
        crate::fs_safe::write_atomic(path, text.as_bytes()).map_err(|err| {
            crate::localization::format(
                "results.import_profiles.save_failed",
                &[
                    ("file_path", &path.display().to_string()),
                    ("error", &err.to_string()),
                ],
            )
        })
    }

    /// Profiles for one target database, in save order. Empty without a
    /// saved connection.
    pub fn for_connection(&self, scope: &ImportScope) -> Vec<&ImportProfile> {
        self.profiles
            .iter()
            .filter(|p| p.scope.same_connection(scope))
            .collect()
    }

    pub fn default_for(&self, scope: &ImportScope) -> Option<&ImportProfile> {
        if !scope.is_saved_connection() {
            return None;
        }
        self.profiles
            .iter()
            .find(|p| p.is_default && p.scope == *scope)
    }

    /// The default profile to auto-apply for `scope` and the loaded source,
    /// if any: the exact target's default first; otherwise the one default
    /// on this connection + database whose saved file pattern, or else whose
    /// saved header columns, match the source. Ambiguous matches apply
    /// nothing (the picker is still there).
    pub fn auto_apply_candidate(
        &self,
        scope: &ImportScope,
        source: &LoadedSource<'_>,
    ) -> Option<&ImportProfile> {
        if !scope.table.is_empty()
            && let Some(profile) = self.default_for(scope)
        {
            return Some(profile);
        }
        let defaults: Vec<&ImportProfile> = self
            .for_connection(scope)
            .into_iter()
            .filter(|p| p.is_default)
            .collect();
        fn single(matches: Vec<&ImportProfile>) -> Option<&ImportProfile> {
            match matches.as_slice() {
                [only] => Some(*only),
                _ => None,
            }
        }
        if let Some(pattern) = source.file_name.and_then(source_pattern) {
            let by_pattern: Vec<&ImportProfile> = defaults
                .iter()
                .copied()
                .filter(|p| p.source_pattern.as_deref() == Some(pattern.as_str()))
                .collect();
            if !by_pattern.is_empty() {
                return single(by_pattern);
            }
        }
        if source.columns.is_empty() {
            return None;
        }
        single(
            defaults
                .into_iter()
                .filter(|p| {
                    p.source_columns.len() == source.columns.len()
                        && p.source_columns
                            .iter()
                            .zip(source.columns)
                            .all(|(a, b)| a.eq_ignore_ascii_case(b))
                })
                .collect(),
        )
    }

    #[cfg(test)]
    pub fn get(&self, id: uuid::Uuid) -> Option<&ImportProfile> {
        self.profiles.iter().find(|p| p.id == id)
    }

    /// Insert or replace: same id wins, then same (scope, name) — saving a
    /// profile under an existing name for the same target overwrites it.
    pub fn upsert(&mut self, profile: ImportProfile) {
        if let Some(existing) = self.profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile;
            return;
        }
        if let Some(existing) = self
            .profiles
            .iter_mut()
            .find(|p| p.scope == profile.scope && p.name.eq_ignore_ascii_case(&profile.name))
        {
            let mut profile = profile;
            profile.id = existing.id;
            profile.is_default = existing.is_default;
            *existing = profile;
            return;
        }
        self.profiles.push(profile);
    }

    pub fn remove(&mut self, id: uuid::Uuid) {
        self.profiles.retain(|p| p.id != id);
    }

    pub fn rename(&mut self, id: uuid::Uuid, name: &str) -> Result<(), String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(crate::localization::text(
                "results.import_profiles.name_required",
            ));
        }
        let Some(index) = self.profiles.iter().position(|p| p.id == id) else {
            return Err(crate::localization::text(
                "results.import_profiles.no_longer_exists",
            ));
        };
        let scope = self.profiles[index].scope.clone();
        if self
            .profiles
            .iter()
            .any(|p| p.id != id && p.scope == scope && p.name.eq_ignore_ascii_case(name))
        {
            return Err(crate::localization::format(
                "results.import_profiles.duplicate_name",
                &[("profile_name", name)],
            ));
        }
        self.profiles[index].name = name.to_string();
        Ok(())
    }

    /// Mark one profile the default for its scope, clearing every other
    /// default for the same exact target. `false` just clears.
    pub fn set_default(&mut self, id: uuid::Uuid, is_default: bool) {
        let Some(scope) = self
            .profiles
            .iter()
            .find(|p| p.id == id)
            .map(|p| p.scope.clone())
        else {
            return;
        };
        for profile in &mut self.profiles {
            if profile.scope == scope {
                profile.is_default = profile.id == id && is_default;
            }
        }
    }
}

/// The result of rebasing a positional spec onto a file whose columns no
/// longer match the profile's saved source columns: the rebuilt spec plus the
/// saved columns whose entries were dropped, for an honest warning.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RebasedSpec {
    pub spec: String,
    pub dropped: Vec<String>,
    /// New source columns the rebase mapped to `-` (skip) because the target
    /// is an existing table that may not have them.
    pub skipped_new: Vec<String>,
}

/// Shared rebase core. Returns the per-current-column tokens and the dropped
/// saved columns, or `None` when the spec should be applied verbatim (empty
/// spec, unchanged columns, or a saved spec that no longer aligns with its
/// saved column list — rebasing that would guess, so the wizard's own
/// validation reports it instead).
fn rebase_tokens(
    saved_columns: &[String],
    saved_spec: &str,
    current_columns: &[String],
) -> Option<(Vec<Option<String>>, Vec<String>)> {
    if saved_spec.trim().is_empty() {
        return None;
    }
    let tokens: Vec<&str> = saved_spec.split(',').map(str::trim).collect();
    if tokens.len() != saved_columns.len() || saved_columns == current_columns {
        return None;
    }
    let mut dropped = Vec::new();
    for (index, saved) in saved_columns.iter().enumerate() {
        let present = current_columns
            .iter()
            .any(|c| c.eq_ignore_ascii_case(saved));
        if !present {
            let token = tokens.get(index).copied().unwrap_or("");
            if !token.is_empty()
                && !token.eq_ignore_ascii_case("-")
                && !token.eq_ignore_ascii_case("auto")
            {
                dropped.push(saved.clone());
            }
        }
    }
    let chosen = current_columns
        .iter()
        .map(|current| {
            saved_columns
                .iter()
                .position(|s| s.eq_ignore_ascii_case(current))
                .and_then(|index| tokens.get(index).copied())
                .map(str::to_string)
        })
        .collect();
    Some((chosen, dropped))
}

/// Mapping-specific rebase. A new source column maps under its own name when
/// the profile creates the table (`map_new_as_self`), and is skipped (`-`)
/// when it imports into an existing table, which may have no such column.
pub fn rebase_mapping_spec(
    saved_columns: &[String],
    saved_spec: &str,
    current_columns: &[String],
    map_new_as_self: bool,
) -> RebasedSpec {
    let Some((chosen, dropped)) = rebase_tokens(saved_columns, saved_spec, current_columns) else {
        return RebasedSpec {
            spec: saved_spec.trim().to_string(),
            ..RebasedSpec::default()
        };
    };
    let mut skipped_new = Vec::new();
    let spec = chosen
        .into_iter()
        .zip(current_columns)
        .map(|(token, current)| {
            token.unwrap_or_else(|| {
                if map_new_as_self {
                    current.clone()
                } else {
                    skipped_new.push(current.clone());
                    "-".to_string()
                }
            })
        })
        .collect::<Vec<_>>()
        .join(", ");
    RebasedSpec {
        spec,
        dropped,
        skipped_new,
    }
}

/// Type-override rebase: new source columns stay `auto`.
pub fn rebase_types_spec(
    saved_columns: &[String],
    saved_spec: &str,
    current_columns: &[String],
) -> RebasedSpec {
    let Some((chosen, dropped)) = rebase_tokens(saved_columns, saved_spec, current_columns) else {
        return RebasedSpec {
            spec: saved_spec.trim().to_string(),
            ..RebasedSpec::default()
        };
    };
    let spec = chosen
        .into_iter()
        .map(|token| token.unwrap_or_else(|| "auto".to_string()))
        .collect::<Vec<_>>()
        .join(", ");
    RebasedSpec {
        spec,
        dropped,
        skipped_new: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(table: &str) -> ImportScope {
        ImportScope {
            engine: "PostgreSQL".into(),
            connection_id: uuid::Uuid::from_u128(0x5a1f),
            database: "sales".into(),
            schema: "public".into(),
            table: table.into(),
        }
    }

    fn profile(name: &str, scope: ImportScope) -> ImportProfile {
        ImportProfile {
            id: uuid::Uuid::new_v4(),
            name: name.into(),
            scope,
            delimiter: ";".into(),
            has_header: true,
            sheet: None,
            null_token: "NULL".into(),
            mapping_spec: "id, customer_name, -".into(),
            types_spec: "int, text, auto".into(),
            create_table: false,
            bulk_insert: false,
            source_columns: vec!["id".into(), "name".into(), "legacy".into()],
            is_default: false,
            source_pattern: None,
        }
    }

    #[test]
    fn profile_round_trips_as_camel_case_json() {
        let profile = profile("vendor weekly", scope("orders"));
        let text = serde_json::to_string(&profile).expect("serialize");
        assert!(text.contains("\"mappingSpec\":\"id, customer_name, -\""));
        assert!(text.contains("\"connectionId\""));
        let back: ImportProfile = serde_json::from_str(&text).expect("parse");
        assert_eq!(back, profile);
    }

    #[test]
    fn scope_lists_per_connection_and_defaults_per_exact_target() {
        let mut store = ImportProfiles::default();
        let scope_a = scope("orders");
        let scope_b = scope("customers");
        let other_conn = ImportScope {
            connection_id: uuid::Uuid::new_v4(),
            ..scope_a.clone()
        };
        let mut a = profile("a", scope_a.clone());
        let b = profile("b", scope_b.clone());
        a.is_default = true;
        store.upsert(a);
        store.upsert(b);
        store.upsert(profile("other", other_conn));
        assert_eq!(
            store.for_connection(&scope_a).len(),
            2,
            "orders + customers share the connection scope"
        );
        assert_eq!(
            store.default_for(&scope_a).map(|p| p.name.as_str()),
            Some("a")
        );
        assert_eq!(store.default_for(&scope_b), None);
    }

    #[test]
    fn upsert_same_name_same_scope_replaces_and_keeps_default() {
        let mut store = ImportProfiles::default();
        let scope = scope("orders");
        let mut first = profile("weekly", scope.clone());
        first.is_default = true;
        let first_id = first.id;
        store.upsert(first);
        let mut revised = profile("WEEKLY", scope.clone());
        revised.id = uuid::Uuid::new_v4();
        revised.null_token = "NIL".into();
        store.upsert(revised);
        assert_eq!(store.profiles.len(), 1);
        assert_eq!(store.profiles[0].id, first_id);
        assert_eq!(store.profiles[0].null_token, "NIL");
        assert!(store.profiles[0].is_default);
    }

    #[test]
    fn rename_validates_conflicts_and_remove_deletes() {
        let mut store = ImportProfiles::default();
        let scope = scope("orders");
        let a = profile("a", scope.clone());
        let a_id = a.id;
        store.upsert(a);
        store.upsert(profile("b", scope));
        assert!(
            store.rename(a_id, "B").is_err(),
            "case-insensitive conflict"
        );
        assert!(store.rename(a_id, "  ").is_err());
        store.rename(a_id, "renamed").expect("rename");
        assert_eq!(store.get(a_id).expect("get").name, "renamed");
        store.remove(a_id);
        assert!(store.get(a_id).is_none());
    }

    #[test]
    fn set_default_clears_siblings_in_scope_only() {
        let mut store = ImportProfiles::default();
        let scope_a = scope("orders");
        let scope_b = scope("customers");
        let mut a = profile("a", scope_a.clone());
        a.is_default = true;
        let a_id = a.id;
        let b = profile("b", scope_a.clone());
        let b_id = b.id;
        let mut other = profile("other", scope_b);
        other.is_default = true;
        store.upsert(a);
        store.upsert(b.clone());
        store.upsert(other);
        store.set_default(b_id, true);
        assert!(!store.get(a_id).expect("a").is_default);
        assert!(store.get(b_id).expect("b").is_default);
        assert!(
            store.default_for(&scope("customers")).is_some(),
            "another target's default is untouched"
        );
    }

    #[test]
    fn source_pattern_folds_dates_and_numbers() {
        assert_eq!(
            source_pattern("vendor_2026-09-28.csv").as_deref(),
            Some("vendor_#.csv")
        );
        assert_eq!(
            source_pattern("Vendor_2026-10-05.CSV"),
            source_pattern("vendor_2026-09-28.csv")
        );
        assert_eq!(
            source_pattern("report-20260928 1405.xlsx").as_deref(),
            Some("report-#.xlsx")
        );
        assert_eq!(source_pattern("orders.csv").as_deref(), Some("orders.csv"));
        assert_eq!(
            source_pattern("2026-09-28.csv"),
            None,
            "a bare date matches too much"
        );
    }

    fn with_default(mut p: ImportProfile, pattern: Option<&str>) -> ImportProfile {
        p.is_default = true;
        p.source_pattern = pattern.map(str::to_string);
        p
    }

    #[test]
    fn dated_file_finds_its_default_even_when_the_table_prefill_differs() {
        let mut store = ImportProfiles::default();
        store.upsert(with_default(
            profile("vendor", scope("orders")),
            Some("vendor_#.csv"),
        ));
        store.upsert(with_default(
            profile("other", scope("customers")),
            Some("crm_#.csv"),
        ));
        // The wizard prefilled the table from the file stem.
        let wizard = scope("vendor_2026-10-05");
        let columns = vec!["x".to_string()];
        let source = LoadedSource {
            file_name: Some("vendor_2026-10-05.csv"),
            columns: &columns,
        };
        let found = store.auto_apply_candidate(&wizard, &source).expect("match");
        assert_eq!(found.name, "vendor");
        assert_eq!(found.scope.table, "orders");
        // Another connection's wizard never sees it.
        let foreign = ImportScope {
            connection_id: uuid::Uuid::new_v4(),
            ..wizard
        };
        assert!(store.auto_apply_candidate(&foreign, &source).is_none());
    }

    #[test]
    fn clipboard_source_matches_a_default_by_header_columns() {
        let mut store = ImportProfiles::default();
        store.upsert(with_default(profile("paste", scope("orders")), None));
        let wizard = scope("pasted_data_20260928_1405");
        let same = vec!["ID".to_string(), "Name".to_string(), "legacy".to_string()];
        let source = LoadedSource {
            file_name: None,
            columns: &same,
        };
        assert_eq!(
            store
                .auto_apply_candidate(&wizard, &source)
                .map(|p| p.name.as_str()),
            Some("paste")
        );
        let different = vec!["id".to_string()];
        let source = LoadedSource {
            file_name: None,
            columns: &different,
        };
        assert!(store.auto_apply_candidate(&wizard, &source).is_none());
    }

    #[test]
    fn exact_target_default_wins_and_ambiguous_matches_apply_nothing() {
        let mut store = ImportProfiles::default();
        store.upsert(with_default(
            profile("a", scope("orders")),
            Some("feed_#.csv"),
        ));
        store.upsert(with_default(
            profile("b", scope("returns")),
            Some("feed_#.csv"),
        ));
        let columns: Vec<String> = Vec::new();
        let source = LoadedSource {
            file_name: Some("feed_1.csv"),
            columns: &columns,
        };
        assert!(
            store
                .auto_apply_candidate(&scope("staging"), &source)
                .is_none(),
            "two defaults claim the pattern"
        );
        assert_eq!(
            store
                .auto_apply_candidate(&scope("returns"), &source)
                .map(|p| p.name.as_str()),
            Some("b"),
            "typing the table re-checks and finds its default"
        );
    }

    #[test]
    fn nil_connection_has_no_profiles_or_defaults() {
        let mut store = ImportProfiles::default();
        let nil_scope = ImportScope {
            connection_id: uuid::Uuid::nil(),
            ..scope("orders")
        };
        store.upsert(with_default(profile("orphan", nil_scope.clone()), None));
        assert!(store.for_connection(&nil_scope).is_empty());
        assert!(store.default_for(&nil_scope).is_none());
    }

    #[test]
    fn labels_carry_the_target_table() {
        let mut p = profile("weekly", scope("orders"));
        assert_eq!(p.label(), "weekly — public.orders");
        p.is_default = true;
        assert_eq!(p.label(), "weekly — public.orders (default)");
    }

    #[test]
    fn store_refuses_to_overwrite_a_corrupt_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("import-profiles.json");
        assert!(
            ImportProfiles::load_from(&path)
                .expect("missing")
                .profiles
                .is_empty()
        );
        let mut store = ImportProfiles::default();
        store.upsert(profile("a", scope("orders")));
        store.save_to(&path).expect("save");
        assert_eq!(
            ImportProfiles::load_from(&path)
                .expect("load")
                .profiles
                .len(),
            1
        );
        std::fs::write(&path, "[oops").expect("corrupt");
        assert!(ImportProfiles::load_from(&path).is_err());
        assert!(ImportProfiles::default().save_to(&path).is_err());
        assert_eq!(std::fs::read_to_string(&path).expect("read"), "[oops");
    }

    #[test]
    fn new_columns_are_skipped_for_existing_tables() {
        let saved = vec!["id".to_string()];
        let current = vec!["id".to_string(), "extra".to_string()];
        let existing = rebase_mapping_spec(&saved, "customer_id", &current, false);
        assert_eq!(existing.spec, "customer_id, -");
        assert_eq!(existing.skipped_new, vec!["extra"]);
        let create = rebase_mapping_spec(&saved, "customer_id", &current, true);
        assert_eq!(create.spec, "customer_id, extra");
        assert!(create.skipped_new.is_empty());
    }

    #[test]
    fn rebase_verbatim_when_columns_unchanged() {
        let saved = vec!["id".to_string(), "name".to_string()];
        let rebased = rebase_mapping_spec(&saved, "customer_id, display", &saved, false);
        assert_eq!(rebased.spec, "customer_id, display");
        assert!(rebased.dropped.is_empty());
    }

    #[test]
    fn rebase_drops_vanished_columns_and_keeps_unmapped_entries_visible() {
        let saved = vec!["id".to_string(), "name".to_string(), "legacy".to_string()];
        let current = vec!["id".to_string(), "legacy".to_string(), "extra".to_string()];
        let rebased = rebase_mapping_spec(&saved, "customer_id, display_name, -", &current, true);
        // id keeps its mapping, legacy stays skipped, new column maps as itself.
        assert_eq!(rebased.spec, "customer_id, -, extra");
        assert_eq!(rebased.dropped, vec!["name"]);
        let types = rebase_types_spec(&saved, "int, text, auto", &current);
        assert_eq!(types.spec, "int, auto, auto");
        assert_eq!(types.dropped, vec!["name"]);
    }

    #[test]
    fn rebase_skipped_columns_do_not_warn() {
        let saved = vec!["id".to_string(), "junk".to_string()];
        let current = vec!["id".to_string()];
        let rebased = rebase_mapping_spec(&saved, "customer_id, -", &current, false);
        assert_eq!(rebased.spec, "customer_id");
        assert!(
            rebased.dropped.is_empty(),
            "a skipped source column carries no mapping to lose"
        );
    }

    #[test]
    fn rebase_empty_spec_stays_empty() {
        let saved = vec!["id".to_string()];
        assert_eq!(rebase_mapping_spec(&saved, "", &saved, false).spec, "");
        assert_eq!(rebase_types_spec(&saved, "  ", &saved).spec, "");
    }

    #[test]
    fn rebase_misaligned_saved_spec_applies_verbatim() {
        // Hand-edited profile whose spec no longer matches its source columns:
        // rebasing would guess; apply as-is and let wizard validation speak.
        let saved = vec!["id".to_string(), "name".to_string()];
        let current = vec!["id".to_string()];
        let rebased = rebase_mapping_spec(&saved, "only_one", &current, false);
        assert_eq!(rebased.spec, "only_one");
        assert!(rebased.dropped.is_empty());
    }
}
