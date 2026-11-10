//! GPUI hygiene guard: every `src/*.rs` file must be reachable from a `mod`
//! declaration (or a `#[path = "..."]` attribute), so a source file can't sit
//! in the tree looking shipped while never compiling. `find_replace.rs` once
//! landed exactly that way — no `mod find_replace;`, so its tests never ran
//! while the feature docs described it as available.

use std::collections::BTreeSet;
use std::path::Path;

/// Legacy orphans inherited from the Swift-era port. The original twelve
/// were deleted on 2026-08-23 (2,439 lines of never-compiled code; git
/// history keeps them). The list stays as an empty guard: do NOT add to it —
/// new files must be declared when they land.
const KNOWN_LEGACY_ORPHANS: &[&str] = &[];

#[test]
fn every_source_file_is_declared_as_a_module() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut declared = BTreeSet::new();
    let mut stems = BTreeSet::new();

    for entry in std::fs::read_dir(&src).expect("read gpui src dir") {
        let path = entry.expect("dir entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("rs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .expect("utf-8 file stem")
            .to_string();
        let text = std::fs::read_to_string(&path).expect("read source file");
        for line in text.lines() {
            let line = line.trim_start();
            // `mod x;`, `pub mod x;`, `pub(crate) mod x;` (also matches inline
            // `mod x {` blocks, which merely adds names that match no file).
            for prefix in ["mod ", "pub mod ", "pub(crate) mod "] {
                if let Some(rest) = line.strip_prefix(prefix) {
                    let name: String = rest
                        .chars()
                        .take_while(|c| c.is_alphanumeric() || *c == '_')
                        .collect();
                    if !name.is_empty() {
                        declared.insert(name);
                    }
                }
            }
            // `#[path = "x.rs"]` re-routes a mod to another file; the target
            // file is compiled even though no `mod` bears its name.
            if let Some(rest) = line.strip_prefix("#[path = \"")
                && let Some(target) = rest.split('"').next()
                && let Some(target_stem) = target.strip_suffix(".rs")
            {
                declared.insert(target_stem.to_string());
            }
            // `include!("x.rs")` inlines a file's tokens into the including
            // module; the target compiles even though no `mod` bears its name.
            // (e.g. `dump_schedule_impl.rs`, shared by the native and wasm
            // `dump_schedule` modules.)
            if let Some(rest) = line.strip_prefix("include!(\"")
                && let Some(target) = rest.split('"').next()
                && let Some(target_stem) =
                    Path::new(target).file_stem().and_then(|stem| stem.to_str())
            {
                declared.insert(target_stem.to_string());
            }
        }
        stems.insert(stem);
    }

    let orphans: Vec<&String> = stems
        .iter()
        .filter(|stem| !matches!(stem.as_str(), "lib" | "main"))
        .filter(|stem| !declared.contains(*stem))
        .filter(|stem| !KNOWN_LEGACY_ORPHANS.contains(&stem.as_str()))
        .collect();
    assert!(
        orphans.is_empty(),
        "orphan module files in client/apps/gpui/src — present on disk but never declared with \
         `mod`, so they do not compile and their tests never run. Declare each in lib.rs (or \
         delete the file): {orphans:?}"
    );

    // The legacy list must only burn down: a wired or deleted entry left here
    // would quietly re-open the loophole the list is meant to contain.
    let stale: Vec<&&str> = KNOWN_LEGACY_ORPHANS
        .iter()
        .filter(|stem| !stems.contains(**stem) || declared.contains(**stem))
        .collect();
    assert!(
        stale.is_empty(),
        "KNOWN_LEGACY_ORPHANS entries that are now wired or deleted — remove them from the list \
         in client/apps/gpui/tests/orphan_modules.rs: {stale:?}"
    );
}
