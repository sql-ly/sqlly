//! Stable per-object hashing.
//!
//! The hash is the source of truth for "did this object's body change?". A
//! timestamp gate decides whether we bother recomputing it, but the final
//! verdict is always the hash: equal hash ⇒ no change (even if the timestamp
//! moved), which eliminates false positives.
//!
//! We hash a canonical byte representation built from the object's identity and
//! body — **not** its provenance or timestamps. Two objects with identical
//! structure but different `modify_date` therefore hash the same, which is
//! exactly what we want.

use sha2::{Digest, Sha256};

use crate::catalog::wire::{CatalogColumn, CatalogForeignKey, CatalogObject, CatalogObjectKind};

/// Compute a stable hex SHA-256 over the canonical form of an object.
///
/// Canonicalization rules (order matters — changing any of these changes the
/// hash for every object, so they are part of the contract):
/// - identity: `kind\n schema-qualifier \n name` (lower-cased)
/// - columns: sorted by `(ordinal, name)`, each emitted as `name|type|nullable`
/// - foreign keys: sorted by `(target_table, first column)`, emitted as
///   `cols -> target(cols)`
/// - definition: included verbatim when present
///
/// Provenance, timestamps, and source path are deliberately excluded.
pub fn object_hash(obj: &CatalogObject) -> String {
    let mut hasher = Sha256::new();

    // Identity.
    hasher.update(CANONICAL_VERSION);
    hasher.update(kind_token(obj.kind).as_bytes());
    hasher.update(b"\n");
    match &obj.schema {
        Some(s) => {
            hasher.update(s.to_ascii_lowercase().as_bytes());
        }
        None => hasher.update(b"\x00"),
    }
    hasher.update(b"\n");
    hasher.update(obj.name.to_ascii_lowercase().as_bytes());
    hasher.update(b"\n");

    // Columns (stable order: by ordinal then name).
    let mut cols: Vec<&CatalogColumn> = obj.columns.iter().collect();
    cols.sort_by(|a, b| a.ordinal.cmp(&b.ordinal).then_with(|| a.name.cmp(&b.name)));
    for c in &cols {
        hasher.update(c.name.to_ascii_lowercase().as_bytes());
        hasher.update(b"|");
        match &c.type_name {
            Some(t) => hasher.update(t.trim().to_ascii_lowercase().as_bytes()),
            None => hasher.update(b"\x00"),
        }
        hasher.update(b"|");
        hasher.update(if c.nullable { b"1" } else { b"0" });
        hasher.update(b"\n");
    }
    // Sentinel so adding/removing a trailing column is detected even when both
    // sides hash to the same prefix.
    hasher.update(SECTION_END);

    // Primary key (declaration order is significant: it is the key column order).
    for col in &obj.primary_key {
        hasher.update(col.to_ascii_lowercase().as_bytes());
        hasher.update(b"\n");
    }
    hasher.update(SECTION_END);

    // Foreign keys (stable order).
    let mut fks: Vec<&CatalogForeignKey> = obj.foreign_keys.iter().collect();
    fks.sort_by(|a, b| {
        a.target_table
            .to_ascii_lowercase()
            .cmp(&b.target_table.to_ascii_lowercase())
            .then_with(|| {
                a.columns
                    .first()
                    .map(String::as_str)
                    .unwrap_or("")
                    .to_ascii_lowercase()
                    .cmp(
                        &b.columns
                            .first()
                            .map(String::as_str)
                            .unwrap_or("")
                            .to_ascii_lowercase(),
                    )
            })
    });
    for fk in &fks {
        for col in &fk.columns {
            hasher.update(col.to_ascii_lowercase().as_bytes());
            hasher.update(b",");
        }
        hasher.update(b"->");
        hasher.update(fk.target_table.to_ascii_lowercase().as_bytes());
        hasher.update(b"(");
        for col in &fk.target_columns {
            hasher.update(col.to_ascii_lowercase().as_bytes());
            hasher.update(b",");
        }
        hasher.update(b")\n");
    }
    hasher.update(SECTION_END);

    // Routine parameters (preserve declaration order — it is semantically
    // significant for routines).
    for p in &obj.parameters {
        hasher.update(p.name.to_ascii_lowercase().as_bytes());
        hasher.update(b"|");
        match &p.type_name {
            Some(t) => hasher.update(t.trim().to_ascii_lowercase().as_bytes()),
            None => hasher.update(b"\x00"),
        }
        hasher.update(b"|");
        if p.is_output {
            hasher.update(b"out");
        } else {
            hasher.update(b"in");
        }
        hasher.update(b"\n");
    }
    hasher.update(SECTION_END);

    // Definition text (views/routines).
    match &obj.definition {
        Some(def) => hasher.update(def.as_bytes()),
        None => hasher.update(b"\x00"),
    }

    hex(&hasher.finalize())
}

/// Bump when the canonicalization rules above change. Folded into every hash so
/// a format change invalidates every stored hash automatically.
const CANONICAL_VERSION: &[u8] = b"sqlly-catalog-hash-v2\n";
const SECTION_END: &[u8] = b"\xE2\x90\x9E\n";

fn kind_token(kind: CatalogObjectKind) -> &'static str {
    kind.as_str()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(hex_digit(b >> 4));
        out.push(hex_digit(b & 0x0f));
    }
    out
}

fn hex_digit(n: u8) -> char {
    match n {
        0..=9 => (b'0' + n) as char,
        _ => (b'a' + (n - 10)) as char,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::wire::{CatalogSource, SourceKind};

    fn table(name: &str, cols: &[(&str, &str)]) -> CatalogObject {
        CatalogObject {
            schema: Some("dbo".into()),
            name: name.into(),
            kind: CatalogObjectKind::Table,
            columns: cols
                .iter()
                .enumerate()
                .map(|(i, (n, t))| CatalogColumn {
                    name: (*n).into(),
                    type_name: Some((*t).into()),
                    ordinal: i as i64 + 1,
                    nullable: false,
                })
                .collect(),
            primary_key: vec![],
            foreign_keys: vec![],
            parameters: vec![],
            definition: None,
            source: CatalogSource {
                kind: SourceKind::Database,
                path: None,
                create_date: None,
                modify_date: None,
            },
        }
    }

    #[test]
    fn identical_objects_hash_equal() {
        assert_eq!(
            object_hash(&table("x", &[("id", "INT")])),
            object_hash(&table("x", &[("id", "INT")]))
        );
    }

    #[test]
    fn hash_is_case_insensitive_for_identity_and_columns() {
        let lower = table("customers", &[("id", "int")]);
        let upper = CatalogObject {
            name: "Customers".into(),
            columns: vec![CatalogColumn {
                name: "ID".into(),
                type_name: Some("INT".into()),
                ordinal: 1,
                nullable: false,
            }],
            ..table("customers", &[])
        };
        assert_eq!(object_hash(&lower), object_hash(&upper));
    }

    #[test]
    fn changing_a_column_type_changes_the_hash() {
        let a = table("x", &[("id", "INT")]);
        let b = table("x", &[("id", "BIGINT")]);
        assert_ne!(object_hash(&a), object_hash(&b));
    }

    #[test]
    fn adding_a_column_changes_the_hash() {
        let a = table("x", &[("id", "INT")]);
        let b = table("x", &[("id", "INT"), ("name", "VARCHAR(50)")]);
        assert_ne!(object_hash(&a), object_hash(&b));
    }

    #[test]
    fn changing_nullability_changes_the_hash() {
        let mut a = table("x", &[("id", "INT")]);
        let mut b = a.clone();
        b.columns[0].nullable = true;
        assert_ne!(object_hash(&a), object_hash(&b));
        a.columns[0].nullable = true;
        assert_eq!(object_hash(&a), object_hash(&b));
    }

    #[test]
    fn timestamps_and_source_path_do_not_affect_the_hash() {
        let mut a = table("x", &[("id", "INT")]);
        a.source.create_date = Some(1);
        a.source.modify_date = Some(2);
        a.source.path = Some("profile-A".into());
        let mut b = table("x", &[("id", "INT")]);
        b.source.create_date = Some(999);
        b.source.modify_date = Some(9999);
        b.source.path = Some("profile-B".into());
        assert_eq!(object_hash(&a), object_hash(&b));
    }

    #[test]
    fn column_order_does_not_affect_hash() {
        let a = table("x", &[("id", "INT"), ("name", "VARCHAR(50)")]);
        // Reverse declaration order — should hash equal because columns are
        // canonicalized by ordinal, but here we swap ordinals too.
        let swapped = CatalogObject {
            columns: vec![
                CatalogColumn {
                    name: "name".into(),
                    type_name: Some("VARCHAR(50)".into()),
                    ordinal: 1,
                    nullable: false,
                },
                CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 2,
                    nullable: false,
                },
            ],
            ..table("x", &[])
        };
        // Same set, different declaration order but ordinals track position, so
        // a's id=1/name=2 vs swapped's name=1/id=2 → different canonical order →
        // different hash. This documents that ordinal is what matters.
        let with_swapped_ordinals = CatalogObject {
            columns: vec![
                CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 2,
                    nullable: false,
                },
                CatalogColumn {
                    name: "name".into(),
                    type_name: Some("VARCHAR(50)".into()),
                    ordinal: 1,
                    nullable: false,
                },
            ],
            ..table("x", &[])
        };
        // a: id@1, name@2 ; with_swapped_ordinals: name@1, id@2 → different
        assert_ne!(object_hash(&a), object_hash(&with_swapped_ordinals));
        // Reorder to match a's ordinals:
        let reordered = CatalogObject {
            columns: vec![
                CatalogColumn {
                    name: "name".into(),
                    type_name: Some("VARCHAR(50)".into()),
                    ordinal: 2,
                    nullable: false,
                },
                CatalogColumn {
                    name: "id".into(),
                    type_name: Some("INT".into()),
                    ordinal: 1,
                    nullable: false,
                },
            ],
            ..table("x", &[])
        };
        assert_eq!(object_hash(&a), object_hash(&reordered));
        // swapped (name@1,id@2) equals with_swapped_ordinals regardless of vec order
        assert_eq!(object_hash(&swapped), object_hash(&with_swapped_ordinals));
    }

    #[test]
    fn hash_is_hex_and_64_chars() {
        let h = object_hash(&table("x", &[("id", "INT")]));
        assert_eq!(h.len(), 64);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn renaming_object_changes_hash() {
        let a = table("x", &[("id", "INT")]);
        let b = table("y", &[("id", "INT")]);
        assert_ne!(object_hash(&a), object_hash(&b));
    }
}
