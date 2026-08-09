//! Static builtin ID catalog contract tests.
//!
//! These tests re-parse the authoritative catalog
//! (`src/builtins/catalog.rs`) and the checked-in no-std mirror
//! (`pd-vm-nostd/src/generated_builtin_ids.rs`) as plain text and pin the
//! contract that `build.rs` enforces at build time:
//!
//! - every VM-visible builtin has exactly one explicit `u16` ID;
//! - IDs, source names, and Rust variants are unique across the catalog;
//! - IDs live in their documented blocks, reserved sentinel ranges stay
//!   empty, and no ID overlaps the bytecode opcode space;
//! - the generated std enum and the public reverse lookup agree with the
//!   catalog one-to-one;
//! - the checked-in no-std mirror is in sync with the std catalog (frozen);
//! - appending or reordering catalog entries cannot renumber existing IDs
//!   (IDs are explicit and immutable once assigned).
#![cfg(feature = "runtime")]

use std::collections::HashMap;

use vm::{BUILTIN_CATALOG, builtin_call_index};

/// Documented call-index blocks; must match `src/builtins/catalog.rs`.
/// Extension block: 0x0000..=0xFF8F (reserved for future builtins and host
/// imports).
const EXTENSION_BLOCK_END: u16 = 0xFF8F;
const SPECIAL_CALL_BLOCK_START: u16 = 0xFF90;
const SPECIAL_CALL_BLOCK_END: u16 = 0xFFA1;
const ORDINARY_BLOCK_START: u16 = 0xFFA2;

/// Reserved sentinel gap inside the special-call block (see the catalog docs
/// and `core.rs::internal_builtins_have_unique_reserved_call_indices`).
const RESERVED_GAP_START: u16 = 0xFF90;
const RESERVED_GAP_END: u16 = 0xFF92;

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogEntry {
    id: u16,
    source_name: String,
    variant: String,
    class: String,
    feature_gate: String,
}

fn catalog_source() -> String {
    std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/builtins/catalog.rs"
    ))
    .expect("read authoritative catalog")
}

fn parse_catalog(source: &str) -> Vec<CatalogEntry> {
    let mut entries = Vec::new();
    for (line_index, raw_line) in source.lines().enumerate() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        let line_number = line_index + 1;
        let rest = line
            .strip_prefix("builtin_id!(")
            .unwrap_or_else(|| panic!("{line_number}: unexpected catalog line: {line:?}"));
        let rest = rest
            .strip_suffix(");")
            .unwrap_or_else(|| panic!("{line_number}: catalog entry must end with ');': {line:?}"));
        let parts: Vec<&str> = rest.split(',').map(str::trim).collect();
        assert_eq!(
            parts.len(),
            5,
            "{line_number}: catalog entry needs 5 fields"
        );
        let id = u16::from_str_radix(parts[0].trim_start_matches("0x"), 16)
            .unwrap_or_else(|err| panic!("{line_number}: invalid id: {err}"));
        entries.push(CatalogEntry {
            id,
            source_name: parts[1].trim_matches('"').to_string(),
            variant: parts[2].to_string(),
            class: parts[3].to_string(),
            feature_gate: parts[4].to_string(),
        });
    }
    entries
}

fn assert_unique(values: &[String], what: &str) {
    let mut seen = std::collections::HashSet::new();
    for value in values {
        assert!(seen.insert(value.clone()), "duplicate {what}: {value}");
    }
}

/// The frozen static ID map: the checked-in catalog, the generated std enum,
/// the public reverse lookup, and the no-std mirror all agree.
#[test]
fn static_builtin_ids_are_frozen() {
    let entries = parse_catalog(&catalog_source());
    assert!(!entries.is_empty(), "catalog must not be empty");

    // Full-catalog uniqueness: IDs, source names, and Rust variants.
    assert_unique(
        &entries
            .iter()
            .map(|entry| entry.id.to_string())
            .collect::<Vec<_>>(),
        "builtin id",
    );
    assert_unique(
        &entries
            .iter()
            .map(|entry| entry.source_name.clone())
            .collect::<Vec<_>>(),
        "builtin source name",
    );
    assert_unique(
        &entries
            .iter()
            .map(|entry| entry.variant.clone())
            .collect::<Vec<_>>(),
        "builtin variant",
    );

    // Block membership, class rules, and feature gates.
    for entry in &entries {
        assert_eq!(
            entry.feature_gate, "none",
            "feature gate for '{}' must be 'none' (no gated builtins exist yet)",
            entry.source_name
        );
        match entry.class.as_str() {
            "Ordinary" => {
                assert!(
                    (ORDINARY_BLOCK_START..=u16::MAX).contains(&entry.id),
                    "ordinary builtin '{}' id 0x{:04X} outside the ordinary block",
                    entry.source_name,
                    entry.id
                );
                assert!(
                    !entry.source_name.starts_with("__"),
                    "ordinary builtin '{}' must not use the internal '__' prefix",
                    entry.source_name
                );
            }
            "Internal" | "Special" => {
                assert!(
                    (SPECIAL_CALL_BLOCK_START..=SPECIAL_CALL_BLOCK_END).contains(&entry.id),
                    "special-call builtin '{}' id 0x{:04X} outside the special-call block",
                    entry.source_name,
                    entry.id
                );
            }
            other => panic!("unknown catalog class {other:?}"),
        }
        let internal_named = entry.source_name.starts_with("__");
        assert_eq!(
            entry.class == "Internal",
            internal_named,
            "class Internal must match the '__' source-name prefix for '{}'",
            entry.source_name
        );
    }

    // Reserved sentinel ranges stay empty: the extension block (future
    // builtins and host imports) and the 0xFF90..=0xFF92 gap.
    for entry in &entries {
        assert!(
            entry.id > EXTENSION_BLOCK_END,
            "id 0x{:04X} of '{}' must not fall in the reserved extension block",
            entry.id,
            entry.source_name
        );
        assert!(
            !(RESERVED_GAP_START..=RESERVED_GAP_END).contains(&entry.id),
            "id 0x{:04X} of '{}' falls in the reserved sentinel gap",
            entry.id,
            entry.source_name
        );
    }

    // No static builtin ID may overlap the bytecode opcode space (u8).
    assert!(
        entries.iter().all(|entry| entry.id > u8::MAX as u16),
        "static builtin IDs must not overlap opcodes"
    );

    // Generated enum parity: BUILTIN_CATALOG == catalog one-to-one by ID.
    // `name()` is the internal/runtime name: namespace separators become `_`,
    // and the language `type` builtin is exposed as `type_of` (mirrors
    // `builtin_internal_name` in build.rs).
    let by_id: HashMap<u16, &CatalogEntry> =
        entries.iter().map(|entry| (entry.id, entry)).collect();
    assert_eq!(BUILTIN_CATALOG.len(), entries.len());
    for builtin in BUILTIN_CATALOG {
        let entry = by_id.get(&builtin.call_index()).unwrap_or_else(|| {
            panic!(
                "generated id 0x{:04X} is missing from the catalog",
                builtin.call_index()
            )
        });
        let expected_name = match entry.source_name.as_str() {
            "type" => "type_of".to_string(),
            other => other.replace("::", "_"),
        };
        assert_eq!(builtin.name(), expected_name);
    }

    // The public reverse lookup resolves every catalog source name to its
    // explicit static ID.
    for entry in &entries {
        assert_eq!(
            builtin_call_index(&entry.source_name),
            Some(entry.id),
            "reverse lookup for '{}'",
            entry.source_name
        );
    }
}

/// The checked-in no-std mirror (`pd-vm-nostd/src/generated_builtin_ids.rs`)
/// dispatches on exactly the same static IDs as the std catalog. This is the
/// sync guard referenced by both files; drift fails here.
#[test]
fn checked_in_nostd_mirror_matches_std_catalog() {
    let entries = parse_catalog(&catalog_source());
    let mirror = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/pd-vm-nostd/src/generated_builtin_ids.rs"
    ))
    .expect("read no-std mirror");

    // Parse `pub const <NAME>_CALL_INDEX: u16 = 0xXXXX;` declarations.
    let mut mirror_ids = Vec::new();
    let mut mirror_by_const = HashMap::new();
    for raw_line in mirror.lines() {
        let line = raw_line.trim();
        let Some(rest) = line.strip_prefix("pub const ") else {
            continue;
        };
        let Some((const_name, value)) = rest.split_once(": u16 = ") else {
            continue;
        };
        let Some(hex) = value.strip_suffix(';') else {
            continue;
        };
        let id = u16::from_str_radix(hex.trim().trim_start_matches("0x"), 16)
            .unwrap_or_else(|err| panic!("mirror const {const_name} has invalid id: {err}"));
        mirror_ids.push(id);
        mirror_by_const.insert(const_name.to_string(), id);
    }
    assert!(
        !mirror_ids.is_empty(),
        "no generated consts found in the no-std mirror"
    );
    assert_eq!(
        mirror_ids.len(),
        entries.len(),
        "mirror/catalog entry count mismatch"
    );

    // Mirror const names are the SCREAMING_SNAKE form of the catalog source
    // names with a `_CALL_INDEX` suffix; every catalog entry must have one
    // mirror const with the identical ID.
    for entry in &entries {
        let expected_const = mirror_const_name(&entry.source_name);
        let mirror_id = mirror_by_const.get(&expected_const).unwrap_or_else(|| {
            panic!(
                "no mirror const {expected_const} for '{}'",
                entry.source_name
            )
        });
        assert_eq!(
            mirror_id, &entry.id,
            "mirror const {expected_const} disagrees with the catalog id of '{}'",
            entry.source_name
        );
    }

    // The mirror's ALL_CALL_INDICES array is the same ascending, unique ID set.
    let array_start = mirror
        .find("pub const ALL_CALL_INDICES")
        .expect("mirror must export ALL_CALL_INDICES");
    let array_ids: Vec<u16> = mirror[array_start..]
        .lines()
        .filter_map(|raw_line| {
            let name = raw_line.trim().strip_suffix(',')?.trim();
            name.ends_with("_CALL_INDEX")
                .then(|| mirror_by_const.get(name).copied())
                .flatten()
        })
        .collect();
    assert_eq!(
        array_ids.len(),
        entries.len(),
        "ALL_CALL_INDICES length mismatch"
    );
    assert!(
        array_ids.windows(2).all(|pair| pair[0] < pair[1]),
        "ALL_CALL_INDICES must be strictly ascending"
    );
    let mut catalog_ids: Vec<u16> = entries.iter().map(|entry| entry.id).collect();
    catalog_ids.sort_unstable();
    assert_eq!(array_ids, catalog_ids);
}

/// Derive the no-std mirror const name from the catalog source name:
/// uppercase the `::`/`_` segments and append `_CALL_INDEX`.
fn mirror_const_name(source_name: &str) -> String {
    let mut parts = Vec::new();
    for segment in source_name.split([':', '_']) {
        if !segment.is_empty() {
            parts.push(segment.to_ascii_uppercase());
        }
    }
    format!("{}_CALL_INDEX", parts.join("_"))
}

/// IDs are explicit and immutable once assigned: reordering the declarations
/// or appending a new entry must not renumber any existing entry.
#[test]
fn appending_or_reordering_catalog_entries_does_not_renumber_existing_ids() {
    let source = catalog_source();
    let entries = parse_catalog(&source);
    let name_to_id: HashMap<&str, u16> = entries
        .iter()
        .map(|entry| (entry.source_name.as_str(), entry.id))
        .collect();

    // Reordering declarations must not renumber: IDs come from the file.
    let mut lines: Vec<&str> = source
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("builtin_id!("))
        .collect();
    lines.reverse();
    let reordered = parse_catalog(&lines.join("\n"));
    assert_eq!(reordered.len(), entries.len());
    for entry in &reordered {
        assert_eq!(
            name_to_id.get(entry.source_name.as_str()),
            Some(&entry.id),
            "reordering renumbered '{}'",
            entry.source_name
        );
    }

    // Appending a new entry at the next free ordinary ID (append-only
    // allocation) must not renumber any existing entry.
    let mut used: Vec<u16> = entries.iter().map(|entry| entry.id).collect();
    used.sort_unstable();
    let next_free = (ORDINARY_BLOCK_START..=u16::MAX)
        .find(|candidate| used.binary_search(candidate).is_err())
        .expect("ordinary block is exhausted");
    let appended = format!(
        "{source}\nbuiltin_id!(0x{next_free:04X}, \"synthetic_contract_probe\", \
         SyntheticContractProbe, Ordinary, none);\n"
    );
    let reparsed = parse_catalog(&appended);
    assert_eq!(reparsed.len(), entries.len() + 1);
    for entry in &reparsed {
        if entry.source_name == "synthetic_contract_probe" {
            assert_eq!(entry.id, next_free);
            assert_eq!(entry.class, "Ordinary");
        } else {
            assert_eq!(
                name_to_id.get(entry.source_name.as_str()),
                Some(&entry.id),
                "append renumbered '{}'",
                entry.source_name
            );
        }
    }
}
