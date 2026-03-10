# Remove Aliases, Backward Compat, and Legacy Behaviors

Full audit of all `pd-*` crates. No `#[deprecated]` attributes, LEGACY/COMPAT comments, or compat feature flags exist in `pd-vm`, [pd-edge](file:///d:/Workspace/project-d/Dockerfile.pd-edge), or `pd-edge-abi`. All found items are in [pd-controller](file:///d:/Workspace/project-d/Dockerfile.pd-controller). One test in `pd-vm` ([assemble_rejects_legacy_opcode_literals](file:///d:/Workspace/project-d/pd-vm/tests/vm/vm_runtime_tests.rs#437-445)) tests that old opcode names stay rejected—that is a regression guard and should **not** be removed.

---

## Summary of Findings

| # | Location | Kind | Description |
|---|----------|------|-------------|
| 1 | [pd-controller/src/server.rs](file:///d:/Workspace/project-d/pd-controller/src/server.rs) L570–579, L999–1037 | Legacy struct + fallback path | [ControllerSnapshotLegacy](file:///d:/Workspace/project-d/pd-controller/src/server.rs#570-580) is a monolithic snapshot format that predates the split into [ControllerCoreSnapshot](file:///d:/Workspace/project-d/pd-controller/src/server.rs#478-490). On startup, if the primary parse fails, the code tries to deserialize as the legacy format. |
| 2 | [pd-controller/src/server.rs](file:///d:/Workspace/project-d/pd-controller/src/server.rs) L546–567 | Optional compat fields | `PersistedEdgeMergedRecord.edge_id` and `.edge_name` are `Option<String>` only to support the old format where the edge name was used as the map key instead of a UUID. The deserialization fix-up logic is in `ControllerStore::from_persisted` (L856–888). |
| 3 | [pd-controller/src/server.rs](file:///d:/Workspace/project-d/pd-controller/src/server.rs) L44–53, L949–952, L1348–1395 | Multi-version binary decode | Timeseries format has 3 versions (`TIMESERIES_SCHEMA_VERSION_V1 = 1`, `V2 = 2`, current `= 3`). The decoder branches on `schema_version >= V2` and `>= V1` to backfill latency fields that didn't exist in older snapshots. |
| 4 | [pd-controller/src/server/ui_codegen.rs](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs) L3718–3767 | Legacy graph field | `string_slice` block reads `length` from saved graph data if [end](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs#2270-2276) is absent, to support graphs saved before [end](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs#2270-2276) replaced `length`. |
| 5 | [pd-controller/src/server/handlers.rs](file:///d:/Workspace/project-d/pd-controller/src/server/handlers.rs) L274–281 | API compat guard | [create_program_version_handler](file:///d:/Workspace/project-d/pd-controller/src/server/handlers.rs#269-341) has an `inferred_code_only` check that ignores `flow_synced` when the client sends a source-only payload (no nodes, no blocks), to accept old-style saves from clients before the flow-sync field existed. |

---

## Proposed Changes

### pd-controller — Legacy snapshot format & persisted types

#### [MODIFY] [server.rs](file:///d:/Workspace/project-d/pd-controller/src/server.rs)

**Item 1 — Delete [ControllerSnapshotLegacy](file:///d:/Workspace/project-d/pd-controller/src/server.rs#570-580) (L569–579)**

Delete the struct entirely:
```rust
// DELETE this entire struct:
struct ControllerSnapshotLegacy {
    schema_version: u32,
    command_sequence: u64,
    program_sequence: u64,
    store: PersistedControllerStore,
}
```

And the entire fallback branch in [load_snapshot_from_disk](file:///d:/Workspace/project-d/pd-controller/src/server.rs#955-1155) (L998–1037):
```rust
// DELETE: the Err(_) match arm on serde_json::from_slice::<ControllerCoreSnapshot>
// that tries ControllerSnapshotLegacy. Replace with just a warn + return defaults.
Err(err) => {
    warn!("failed to parse controller snapshot path={} err={err}", ...);
    return (ControllerStore::default(), 0, 0, HashMap::new(), HashMap::new());
}
```

---

**Item 2 — Remove [PersistedControllerStore](file:///d:/Workspace/project-d/pd-controller/src/server.rs#508-516), [PersistedEdgeMergedRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#546-568), and Option compat fields**

Delete both structs (L507–567). The [ControllerCoreSnapshot](file:///d:/Workspace/project-d/pd-controller/src/server.rs#478-490) already directly holds `HashMap<String, PersistedEdgeCoreRecord>` — [PersistedEdgeMergedRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#546-568) was only used as an intermediate during the legacy load path.

Make `PersistedEdgeCoreRecord.edge_id` and `.edge_name` required (non-optional):
```rust
// BEFORE:
struct PersistedEdgeCoreRecord {
    #[serde(default)] edge_id: Option<String>,
    #[serde(default)] edge_name: Option<String>,
    ...
}
// AFTER:
struct PersistedEdgeCoreRecord {
    edge_id: String,
    edge_name: String,
    ...
}
```

Rewrite `ControllerStore::from_persisted` to accept the snapshot + timeseries directly (drop the [PersistedControllerStore](file:///d:/Workspace/project-d/pd-controller/src/server.rs#508-516) parameter):
```rust
// BEFORE signature:
fn from_persisted(store: PersistedControllerStore, max_result_history: usize) -> ControllerStore

// AFTER signature:
fn from_persisted(
    snapshot: ControllerCoreSnapshot,
    programs: HashMap<String, StoredProgram>,
    timeseries: HashMap<String, PersistedEdgeTimeseriesRecord>,
    max_result_history: usize,
) -> ControllerStore
```
The body drops all the UUID-vs-name fix-up logic and just iterates `snapshot.edges` directly.

Rewrite `EdgeRecord::to_persisted` to return [PersistedEdgeCoreRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#518-536) (not [PersistedEdgeMergedRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#546-568)) and take a non-optional `edge_id: String`.

Rewrite `EdgeRecord::from_persisted` to take [PersistedEdgeCoreRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#518-536) + `Option<&PersistedEdgeTimeseriesRecord>` instead of [PersistedEdgeMergedRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#546-568).

Rewrite `ControllerState::persist_snapshot` — it currently calls `guard.to_persisted()` (the removed `ControllerStore::to_persisted`) and then projects [PersistedEdgeMergedRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#546-568) into [PersistedEdgeCoreRecord](file:///d:/Workspace/project-d/pd-controller/src/server.rs#518-536). Replace with a direct iteration of `guard.edges`:
```rust
// BEFORE (inside persist_snapshot):
let persisted = guard.to_persisted();
let core_edges = persisted.edges.iter().map(|(edge_id, record)| {
    (edge_id.clone(), PersistedEdgeCoreRecord { edge_id: record.edge_id.clone(), ... })
}).collect();
let timeseries_edges = persisted.edges.iter().map(...).collect();
// edge_lookup: persisted.edge_lookup.clone()
// programs: persisted.programs.clone()

// AFTER:
let core_edges = guard.edges.iter()
    .map(|(edge_id, record)| (edge_id.clone(), record.to_persisted(edge_id.clone())))
    .collect::<HashMap<_, _>>();
let timeseries_edges = guard.edges.iter()
    .map(|(edge_id, record)| (edge_id.clone(), PersistedEdgeTimeseriesRecord { ... }))
    .collect::<HashMap<_, _>>();
// edge_lookup: guard.edge_lookup.clone()
// programs: guard.programs.clone()
```

Update [load_snapshot_from_disk](file:///d:/Workspace/project-d/pd-controller/src/server.rs#955-1155) call site — instead of building [PersistedControllerStore](file:///d:/Workspace/project-d/pd-controller/src/server.rs#508-516) from merged edges, pass the snapshot and timeseries directly to `ControllerStore::from_persisted`:
```rust
// BEFORE:
let mut merged_edges = HashMap::new();
for (edge_id, core) in snapshot.edges { merged_edges.insert(..., PersistedEdgeMergedRecord { ... }); }
let store = PersistedControllerStore { edges: merged_edges, edge_lookup: ..., programs };
(ControllerStore::from_persisted(store, max_result_history), ...)

// AFTER:
(ControllerStore::from_persisted(snapshot, programs, timeseries, max_result_history), ...)
```

---

**Item 3 — Remove timeseries V1/V2 compat**

Delete constants (L47–48):
```rust
// DELETE:
const TIMESERIES_SCHEMA_VERSION_V1: u32 = 1;
const TIMESERIES_SCHEMA_VERSION_V2: u32 = 2;
```

Simplify [is_supported_timeseries_schema_version](file:///d:/Workspace/project-d/pd-controller/src/server.rs#949-954) (L949–952) to only accept the current version:
```rust
// BEFORE:
fn is_supported_timeseries_schema_version(value: u32) -> bool {
    value == TIMESERIES_SCHEMA_VERSION_V1
        || value == TIMESERIES_SCHEMA_VERSION_V2
        || value == TIMESERIES_SCHEMA_VERSION
}
// AFTER:
fn is_supported_timeseries_schema_version(value: u32) -> bool {
    value == TIMESERIES_SCHEMA_VERSION
}
```

In [decode_timeseries_snapshot](file:///d:/Workspace/project-d/pd-controller/src/server.rs#1316-1423) (L1316–1410), remove the two conditional backfill blocks. [EdgeTrafficPoint](file:///d:/Workspace/project-d/pd-controller/src/server.rs#1530-1556) and `EdgeTrafficSample` are always read with all latency fields — no zero-init + conditional-read pattern:
```rust
// DELETE these two blocks (present twice, once for point once for sample):
if schema_version >= TIMESERIES_SCHEMA_VERSION_V2 {
    point.latency_p50_ms = cursor.read_u64()?; ...
}
if schema_version >= TIMESERIES_SCHEMA_VERSION {
    point.upstream_latency_p50_ms = cursor.read_u64()?; ...
}
// REPLACE with unconditional reads of all fields (the encoder already always writes them all).
```

---

### pd-controller — UI graph compat

#### [MODIFY] [ui_codegen.rs](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs)

- **Item 4**: Delete the `legacy_length` variable and all four `if let Some(length) = &legacy_length` branches in the `string_slice` block handler (L3718–3767). Replace with the clean [render_slice_expr(... start, end)](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs#3103-3111) paths directly.

### pd-controller — API compat guard

#### [MODIFY] [handlers.rs](file:///d:/Workspace/project-d/pd-controller/src/server/handlers.rs)

- **Item 5**: Remove the `inferred_code_only` block (L274–281). Delete the `let inferred_code_only = ...` line and the conditional `flow_synced` override. Let `flow_synced` come directly from `request.flow_synced`.

---

## Verification Plan

### Automated Tests

There are currently no automated tests for [pd-controller](file:///d:/Workspace/project-d/Dockerfile.pd-controller) business logic (it has no `tests/` directory). Verification is by build and smoke-testing.

**Build check** — confirms no compile errors after each change:
```
cd d:\Workspace\project-d
cargo build -p pd-controller
```

**Full workspace build** (to check no cross-crate breakage):
```
cargo build --workspace
```

**pd-vm tests** (to confirm regression guard is untouched):
```
cargo test -p pd-vm --test vm_tests assemble_rejects_legacy_opcode_literals
```

### Manual Verification

1. Run [pd-controller](file:///d:/Workspace/project-d/Dockerfile.pd-controller) with a fresh (empty) `--state-path`. Confirm it starts without warnings.
2. Verify that saving a program and reloading the controller re-reads it correctly.
3. In the UI, create a graph that uses the `string_slice` block. Confirm it generates correct code with [start](file:///d:/Workspace/project-d/pd-controller/src/server/handlers.rs#1324-1375)/[end](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs#2270-2276) fields and no `length` fallback.
4. Submit a `POST /rpc/v1/programs/{id}/versions` with a [source](file:///d:/Workspace/project-d/pd-controller/src/server/ui_codegen.rs#3164-3207)-only payload (no `nodes`, no [blocks](file:///d:/Workspace/project-d/pd-controller/src/server/handlers.rs#124-129), `flow_synced: false`) and confirm it is still accepted (this was always valid; the compat guard we're removing only silently mutated `flow_synced`, which was already [false](file:///d:/Workspace/project-d/pd-vm/tests/vm/vm_runtime_tests.rs#61-79) in that payload).
