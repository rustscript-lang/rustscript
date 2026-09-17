# Host descriptor / resource-effects organization migration report

Audit date: 2026-09-18. Scope: `rustscript-lang` host-descriptor and resource-effects
migration against frozen core `b1d6cffede77f49410bf63525f30b9a46b02dc01`. This
document records the organization inventory, remote candidate verification,
mechanical pin audit, recovered gates, and compatibility decision. It does not
retarget the frozen SHA or change core code.

**Matrix result:** 19 organization repositories (18 public + private
`ngx-rustscript`). No unclassified consumer. Every downstream candidate ref
resolves to the expected SHA; the frozen core SHA resolves through merged PR #30
because its source branch was deleted after merge. No live Cargo/lock/CI stale
pin exists on a migration candidate. Legacy builder and low-level registry APIs
remain for the transition cycle; removal is a separate follow-up after this
matrix, not part of this report.

## 1. Frozen core

| Item | Value |
|---|---|
| Frozen SHA | `b1d6cffede77f49410bf63525f30b9a46b02dc01` |
| Remote provenance | PR #30 head; source branch deleted after merge |
| PR | rustscript-lang/rustscript#30 `refactor(*): host descriptors resource effects`, merged 2026-09-17T08:30:48Z |
| Default `master` | `fd405fbe3c91f5c7fe77762b69ceb81caea6962d` (squash/merge result with the same reviewed tree, not the frozen head commit) |

Downstream candidates pin the frozen **head** SHA, not the merge commit.

At the frozen SHA:

- `RegexCache` lives in `src/builtins/runtime/regex/cache.rs`.
- `src/vm/regex_cache.rs` is absent.
- Generic host state is `src/vm/host_state.rs`.
- Authoring guide: [`docs/host-sdk-descriptors.md`](host-sdk-descriptors.md).

Core `Cargo.lock` still records optional crates.io `pd-edge-abi 0.1.1` and
`pd-host-function 0.22.2`: `pd-vm` retains the optional `edge_abi` feature and
`pd-vm-wasm` retains an unconditional crates.io `pd-edge-abi` dependency in the
frozen tree. `pd-edge` `5f4f889e349bdfbd5534deb42bd13b616a6114f5`
explicitly does **not** enable `vm/edge-abi`; HTTP and every migrated downstream
stay on the in-tree ABI25 family. `scripts/test_publish_crates.py` still mentions
registry `pd-host-function` `0.22.7` as a publish-rewrite fixture. These are
classified as frozen-core leftover graph / publish templates, not consumer stale
pins, and retiring the unused core graph remains a follow-up.

## 2. Organization inventory

Authenticated `GET /orgs/rustscript-lang/repos?per_page=100` (paginated) returned
**19** repositories, none archived. Unauthenticated the same endpoint returned
**18** public repositories and omitted private `ngx-rustscript`; unauthenticated
`GET /repos/rustscript-lang/ngx-rustscript` is **404**. `ngx-rustscript` remains
in-scope as a named host extension; its candidate ref was verified with
authenticated API.

No additional public or private repository appeared that looks like a
RustScript consumer outside the plan matrix.

| Repository | Visibility | Default branch | Default HEAD (2026-09-18) | Role |
|---|---|---|---|---|
| rustscript | public | master | `fd405fbe3c91f5c7fe77762b69ceb81caea6962d` | core |
| ngx-rustscript | private | master | `e5ee7cf88d1a321a482cb45f645139dc9e816efb` | first-order host extension |
| rustscript-agent | public | master | `2bf5660b9c1c35acf6f5650c80fa32ae410a89e9` | first-order host extension |
| pd-edge | public | master | `ff75b42767029e6647a30ca9ac410ed79a6f84ed` | generated ABI / edge runtime |
| flint | public | master | `a8d1711d40e9d8e26068c893b3766b613e311866` | macro example |
| rustscript-bevy-gameplay | public | master | `5b8589247948d4a0904d374948481cd874a2f483` | macro example |
| rustscript-pingora-gateway | public | master | `439817d9865d24eee61b0fdee62292e8ec3324ef` | macro example |
| rustscript-egui-ui | public | master | `dda175d25dedc21123f80f05ca3a773e755ad126` | VM example |
| rustscript-gpui-notepad | public | main | `d68714f499343bb0560097862f19e9d5b2cb77b1` | VM example |
| rustscript-compat-frontends | public | master | `cef2de90a08aa119d08f2053bce58ac98caab98c` | VM/compiler consumer |
| micro-rustscript | public | master | `6cafab20df6b405bdc9b30a5f4bfc3140251da01` | embedded consumer |
| playground | public | master | `f9672929e841eafc89600467a2eaba21838e9a35` | WASM/web consumer |
| pd-controller | public | master | `d02e12aaccf0174cd5c09d0aa0957b0ad84db677` | second-order pd-edge |
| pd-edge-waf | public | main | `ce5c908dc30b83147b7d6b98fc0d1a128c324fad` | second-order pd-edge |
| IronRust | public | master | `720eb7e9354c66e5c90442c0f57f3e8331d83359` | second-order pd-edge / CLR |
| website | public | main | `93a7d85892d719a8e36368823f6faefa43eb23db` | docs |
| spec | public | master | `0a28614f7c0024e6d8636768e898de9a689ee53b` | spec (no-change) |
| linguist | public (fork) | main | `e9fe3c9f230cd9220afcd057f75702de4d7700c9` | syntax metadata (no-change) |
| .github | public | main | `46313ea73fa5613d80fff478a4f5414442129e40` | org profile (no-change) |

Default-branch HEADs are **not** the migration candidates except for the three
no-change repositories (`spec`, `linguist`, `.github`). Merging candidates onto
defaults is outside this report.

## 3. Candidate remote verification

Every downstream candidate was re-read with
`GET /repos/rustscript-lang/{repo}/git/ref/heads/{branch}` (and HTTPS
`git ls-remote` where used); all matched. The frozen core source branch was
deleted after PR #30 merged, so its SHA was verified through the PR head and
commit APIs instead of a live branch ref.

| Repository | Candidate branch / provenance | Remote SHA | Match |
|---|---|---|---|
| rustscript | PR #30 head (source branch deleted after merge) | `b1d6cffede77f49410bf63525f30b9a46b02dc01` | yes |
| pd-edge | `subagent/pd-edge-remove-legacy-abi-feature-12e2b155` | `5f4f889e349bdfbd5534deb42bd13b616a6114f5` | yes |
| ngx-rustscript | `subagent/ngx-host-descriptor-migration-e0996f6d` | `3735344a3e39a3be5f6cfb6f0af22367c3efe8a5` | yes |
| rustscript-agent | `subagent/agent-host-descriptor-migration-37e396a7` | `141b7c3a0c7afcd6806d8e7769bff23146fd4947` | yes |
| flint | `subagent/flint-host-descriptor-migration-18c52491` | `e6893b074177f7e89607305da48cd0cfe60109c8` | yes |
| rustscript-bevy-gameplay | `subagent/bevy-host-descriptor-migration-09881317` | `2dc85580223ef6457aae8bb86583a88fa9340f56` | yes |
| rustscript-pingora-gateway | `subagent/pingora-host-descriptor-migration-aeab8045` | `fb6dc13e9619309c9b8e0520b763568b61754760` | yes |
| rustscript-egui-ui | `subagent/egui-consumer-migration-46f2e831` | `a8c7798095fae31f958422c81d702464135e3ae7` | yes |
| rustscript-gpui-notepad | `subagent/gpui-consumer-migration-20636ed0` | `87bba21ce21bebcabd2de74041d844b970d0a8fb` | yes |
| rustscript-compat-frontends | `subagent/compat-frontends-migration-fd3423ca` | `32d5e0a4c4595c6a4f0ada011ce6c6c5fd4dc386` | yes |
| micro-rustscript | `subagent/micro-consumer-migration-1436cc66` | `d94c0debb44017677f0730bcc6625479e1ba6927` | yes |
| playground | `subagent/playground-consumer-migration-137b1236` | `19a75f56d48f72b7818cb931b08b7490083228eb` | yes |
| pd-controller | `subagent/pd-controller-migration-fc848129` | `b884a4e1ce0a45d93e482174373346c5563e70a0` | yes |
| pd-edge-waf | `subagent/pd-edge-waf-repin-6a8e3d00` | `6977e0d62be3057aae1ed9c7fba92eaec3bb6571` | yes |
| IronRust | `subagent/ironrust-migration-925494a4` | `dfaf0098c202f94da4c07fd1aadd927ef47cbc54` | yes |
| website | `subagent/task15-doc-audit-7203495c` | `bec6f451cd5cd4a454df8f7333118239293af701` | yes |
| spec | `master` | `0a28614f7c0024e6d8636768e898de9a689ee53b` | yes |
| linguist | `main` | `e9fe3c9f230cd9220afcd057f75702de4d7700c9` | yes |
| .github | `main` | `46313ea73fa5613d80fff478a4f5414442129e40` | yes |

SSH `git ls-remote` is not authoritative in this environment (connection closed);
GitHub REST and HTTPS `ls-remote` were.

## 4. Pin / dependency audit

Trees were inspected at the candidate SHAs via `git show SHA:path` / tree listing
without checking out into live `master`/`main`. Search surface: `Cargo.toml`,
`Cargo.lock`, package manifests, CI workflows/scripts, and pin files.

**Live dependency pins on migration candidates:** aligned.

| Consumer class | Proven sources |
|---|---|
| First-order git consumers | `pd-vm` / `pd-host-function` / `pd-host-schema` (and `pd-vm-nostd` where used) → git `b1d6cffede77f49410bf63525f30b9a46b02dc01` |
| Second-order (`pd-controller`, `pd-edge-waf`, `IronRust`) | `pd-edge` / `pd-edge-abi` / `pd-edge-host-function` → git `5f4f889e349bdfbd5534deb42bd13b616a6114f5`; core family → `b1d6cff…` |
| Playground | `scripts/rustscript-core-revision` = full `b1d6cff…`; WASM build asserts that checkout |
| `pd-edge` itself | git core `b1d6cff…`; in-tree `pd-edge-abi` `0.1.0`; no crates.io `pd-edge-abi 0.1.1` / `pd-host-function 0.22.x` |

No candidate lockfile still pins pre-migration core SHAs, abbreviated old revs
such as `0b18a33`, stale pd-edge `6320847` as a **live** git source, path/sibling
absolute deps, or moving `branch =` pins on the rustscript/pd-edge family.

Hits that are **not** live pins:

| Location | Why it is not a stale pin |
|---|---|
| Frozen core `pd-vm` optional `edge_abi` plus `pd-vm-wasm` crates.io `pd-edge-abi 0.1.1` / `pd-host-function 0.22.2` graph | frozen-core leftover; migrated downstream candidates do not enable or inherit it |
| `scripts/test_publish_crates.py` `0.22.7` | publish-rewrite test fixture |
| ngx `README.md` / scheduler design notes `f5f71ebc…` | historical docs; `Cargo.toml` `rustscript-rev` is `b1d6cff…`; `tests/core_pin.rs` treats `f5f71ebc…` as `STALE_REV` |
| agent vendored comments `f9ca4143…` and `STALE_REV` | provenance / negative pin test |
| IronRust `docs/clr-callable-runtime-plan.md` `b3b481ef…` | historical plan text |
| flint `koharu-*` `branch = "refactor/0705"` | third-party koharu, not rustscript/pd-edge |
| linguist `samples/TOML/filenames/Gopkg.lock` `branch = "master"` | Linguist sample, not a RustScript pin |
| pd-edge / pd-controller `plans/*.md` and similar `/home/…` strings | historical plan prose |
| website `plans/documentation-migration.md` | historical plan prose |

**Stale-pin result:** none on classified migration candidates. Residual
documentation that still *names* old SHAs is called out above and is not a
dependency-graph blocker.

## 5. Per-repository outcomes

Relationship and treatment follow the plan matrix. Review verdicts and gate
counts below are recovered from the Task 9–15 implementation/review summaries
for the candidate SHAs. This audit did not re-run Cargo.

### Core and first-order

| Repo | Inspected SHA | Treatment | Remote | Pins | Evidence |
|---|---|---|---|---|---|
| rustscript | `b1d6cff…` | migration (frozen) | PR #30 head | in-tree path family; optional crates.io `edge_abi` leftover classified above | Whole-change review `passed=true` on `82575ac..b1d6cff`. Regex cache moved out of `src/vm/**`. Descriptors/effects, named structs, exact binding, VM reuse, JIT/AOT/async covered in that core review. |
| pd-edge | `5f4f889…` | migration | verified | core git `b1d6cff…`; in-tree ABI25; lock dropped crates.io `pd-edge-abi 0.1.1` / `pd-host-function 0.22.5` | ABI 24→25 then `http` no longer enables `vm/edge-abi`. Review of `5f4f889` vs `6320847`: `passed=true`. Named `MqttEvent`. RSS/examples compiled in Task 9; fmt/clippy/test/release-bin gates recovered as run on the ABI25 path. |
| ngx-rustscript | `3735344…` | migration | verified | `package.metadata.ngx-rustscript.rustscript-rev` and `pd-vm`/`pd-host-function` git `b1d6cff…` | Review `passed=true`. **199** hosts (195 macro + 4 timer), **8** named structs, allowlist **77**, schema v2 fingerprint `0x10c407b2b827eaff`. Production bind is one restricted exact registry (`ngx_host_registry` → `bind_vm_cached`). |
| rustscript-agent | `141b7c3…` | migration | verified | `pd-vm`/`pd-host-function` git `b1d6cff…`; lock matches | Review `passed=true` (five commits vs `2bf5660`). Standard HTTP/SQLite composed with restricted agent registry; storage/tools RSS migrated; pin tests present. Earlier implementation attempts timed out; the reviewed candidate is this SHA. |
| flint | `e6893b0…` | migration | verified | `pd-vm`/`pd-host-function` git `b1d6cff…` | Review `passed=true`. One composition: **13** modules, **207** hosts (105 macro + 102 residual). `install_flint_host_modules` is the production registry writer. |
| rustscript-bevy-gameplay | `2dc8558…` | migration | verified | git `b1d6cff…` | Review `passed=true`. Modules `bevy.world` / `bevy.shooter` / `bevy.gomoku` / `bevy.xiangqi`. Fingerprint golden `0x61e3eaf5de92afc7`. Address commit closed rollback/test gaps. |
| rustscript-pingora-gateway | `fb6dc13…` | migration | verified | git `b1d6cff…` | Review `passed=true` vs `439817d`. One ordered `HostModuleDescriptor` list; restricted/exact runtime bind; named `info` hosts in scope. |
| rustscript-egui-ui | `a8c7798…` | migration | verified | git `b1d6cff…`; sibling/path deps removed | Review `passed=true`. One module: `egui::rgb`, `egui::ui_spec` → Named `UiSpec`. CI no longer checks out core. |
| rustscript-gpui-notepad | `87bba21…` | migration | verified | git `b1d6cff…`; machine-absolute path dep removed | Review `passed=true`. **15** RSS-visible hosts in two modules (`ui` 13 + `notepad` 2). Restricted `install_from_catalog` + `bind_vm_cached`. HostState for UI/notepad; reuse reset clears stale callables. |
| rustscript-compat-frontends | `32d5e0a…` | migration | verified | `pd-vm` git `b1d6cff…` | JS/Lua file-module namespace fold addressed after failed reviews; candidate SHA is the closed address. Pin/lock/CI git form proven. |
| micro-rustscript | `d94c0de…` | migration | verified | `pd-vm` / `pd-vm-nostd` git `b1d6cff…` | Review `passed=true` vs `6cafab20`. Firmware guest ABI stays static C `HOST_EXPORTS`. no_std/cross-target compile recovered; **hardware flash not run** (see §7). |
| playground | `19a75f5…` | migration | verified | `scripts/rustscript-core-revision` = `b1d6cff…` | Review `passed=true`. Five RSS examples: `rss-collections-iter-example.rss`, `rss-complex-example.rss`, `rss-ifft-example.rss`, `rss-lrucache-example.rss`, `rss-strings-regex-example.rss`. WASM build honors `CARGO_TARGET_DIR` and refuses a mismatched core HEAD. |

### Second-order pd-edge consumers

| Repo | Inspected SHA | Treatment | Remote | Pins | Evidence |
|---|---|---|---|---|---|
| pd-controller | `b884a4e…` | migration | verified | pd-edge `5f4f889…`, pd-vm `b1d6cff…`; `mqtt = ["edge/mqtt"]` | Follow-up after ABI24 dual-universe / default-off MQTT-WebRTC blockers. Final review `passed=true`. Default-off MQTT/WebRTC compile accepts Ok only when matching imports are `None`. |
| pd-edge-waf | `6977e0d…` | migration | verified | pd-edge `5f4f889…` with `mqtt`, core `b1d6cff…`; lock dropped crates.io `pd-edge-abi 0.1.1` / `pd-host-function 0.22.6` | Review `passed=true`. Exact RSS corpus **36** `rules/*.rss`. Repin from earlier `6320847` candidate. |
| IronRust | `dfaf009…` | migration | verified | pd-edge family `5f4f889…`, core family `b1d6cff…`; no registry `pd-edge*` / `pd-host-*` / `pd-vm*` | Review `passed=true`. Native compiler + CLR; final repinned Release run passed **82/82** (`PdVm.Tests` 78, `PdEdge.Http.Tests` 4), plus native 14 tests and the seven-example matrix. Rebuilt native artifact differed from the pre-repin binary, proving the final CLR matrix used the new dependency graph. |

### Documentation / no-change

| Repo | Inspected SHA | Treatment | Remote | Result |
|---|---|---|---|---|
| website | `bec6f45…` | docs migration | verified | Host descriptor / HostState lifecycle docs corrected; review `passed=true`. |
| spec | `0a28614…` | no-change | verified | See §6. |
| linguist | `e9fe3c9…` | no-change | verified | See §6. |
| .github | `46313ea…` | no-change | verified | See §6. |

## 6. No-change evidence

### spec `0a28614f7c0024e6d8636768e898de9a689ee53b`

Tree:

- `README.md`, `LICENSE`, `.gitignore`, `.github/workflows/ci.yml`
- `language/{lexical,syntax,grammar}.md`, `syntaxes/rustscript.tmLanguage.json`
- `formats/{bytecode,aot}.md`, `runtime/{debugger,host-functions}.md`
- `assets/rustscript-logo-crabstick.png`

Searched `HostState`, `HostFunctionDescriptor`, `resource effect`, and `VMBC` at
that SHA: no text matches (the PNG is binary). `runtime/host-functions.md`
describes namespace/arity/signature/registration at guest-visible height;
descriptors, resource effects, and host-private state are implementation
metadata. `language/*` and `formats/*` were inspected; no VMBC/RSS/guest syntax
diff is required for this freeze. CI workflow only sanity-checks Markdown files.

### linguist `e9fe3c9f230cd9220afcd057f75702de4d7700c9`

- No `RustScript` string in the tree.
- No `RustScript` language key in `lib/linguist/languages.yml`.
- No rustscript grammar / sample / heuristic.
- `.rss` appears only as an **XML** alias/extension (`languages.yml` XML
  `aliases: rss` and `extensions: ".rss"`), i.e. RSS feeds, not RustScript.
- The only `rss` sample path is `samples/Common Lisp/rss.sexp`.
- `samples/TOML/filenames/Gopkg.lock` `branch = "master"` is a Linguist fixture.

Frozen-core host/descriptor work does not require Linguist edits.

### .github `46313ea73fa5613d80fff478a4f5414442129e40`

Tree is only `README.md` and `profile/README.md` (27 lines). No workflows, issue
templates, Cargo pins, or examples. Links: rustscript.org, playground.rustscript.org,
and the public repo table. No pin or syntax change required.

## 7. Cross-matrix conclusions

| Contract | Outcome |
|---|---|
| Descriptor produces schema + adapter | Core + ngx + agent + pd-edge + flint/Bevy/Pingora + GPUI/egui: one explicit module list, no linker inventory |
| Resource borrow/mut/take/create | Migrated where the host uses typed resources (ngx semaphore/shared-dict, agent capabilities, pd-edge, flint). Micro firmware stays the static C export table |
| Private state / regex | Core: regex cache is host-module state, not a `Vm` field. GPUI uses VM-scoped HostState. ngx timer/runtime state preserved |
| Named-struct fixed shapes | ngx 8 named structs; pd-edge `MqttEvent`; egui `UiSpec`; agent/flint/Bevy/Pingora migrated or audited |
| Catalog fingerprint | ngx `0x10c407b2b827eaff`; Bevy `0x61e3eaf5de92afc7`; others lock-proven at frozen SHA |
| Exact / restricted binding | ngx, agent, flint, GPUI, pd-edge install paths reviewed as single restricted/exact registries |
| VM reset / reuse | Core regex reuse; GPUI reset clears stale callables; ngx worker/request tests in Task 10 evidence |
| Interpreter / JIT / AOT / WASM / CLR | Core JIT/AOT in frozen review; playground WASM; micro VMBC/no_std; IronRust final repinned CLR **82/82** plus native tests and seven-example matrix |
| Async / owned dispatch | ngx timer/semaphore; agent HTTP/tools; pd-edge network ABI |

Plan RSS/example corpora accounted for:

| Corpus | Candidate accounting |
|---|---|
| pd-edge 26 `.rss` + examples | Task 9 ABI25 compile path |
| ngx host/integration tests | Task 10; 199-host catalog |
| agent RSS including HTTP/storage | Task 11 reviewed candidate |
| flint 13 host modules | 207-host unified composition |
| Bevy six example assets / shooter-gomoku-xiangqi | Task 12 + address |
| Pingora gateway tests | Task 12 `passed=true` |
| GPUI two RSS files / 15 hosts | Task 13 |
| egui panel policy example | Task 13 |
| compat JS/Lua corpus | Task 13 address SHA |
| micro four RSS programs | Task 13; firmware ABI unchanged |
| playground five RSS examples | listed in §5 |
| pd-controller edge catalog / MQTT | Task 14 final SHA |
| pd-edge-waf 36 `rules/*.rss` | Task 14 repin |
| IronRust seven example paths | CLR matrix + native repin |
| website host SDK docs | Task 15 `bec6f45` |

**Not run (not failures of required compile-only/skip paths):**

- micro-rustscript **hardware flash** / on-device firmware boot.
- Interactive **Windows UI** execution for egui/gpui (compile/test evidence only).

Required compile-only and skip paths are not marked failed.

## 8. Compatibility decision

Legacy `HostApiBuilder::{resource, named_struct, function}` and
`HostFunctionRegistry::register_*` (including exact/static/owned family) remain
public for this transition cycle. See
[`docs/host-sdk-descriptors.md`](host-sdk-descriptors.md#compatibility-window).

Removal of those APIs is a **separate follow-up** after the organization matrix
is green and candidates are integrated. It is not part of this report commit.

## 9. Residual follow-ups (non-blockers)

- Merge candidate branches onto default branches (defaults still pre-migration).
- After merge: remove the frozen core leftover crates.io edge ABI graph if
  `pd-vm`'s optional `edge_abi` feature and `pd-vm-wasm`'s direct dependency are
  retired.
- Pin CI sibling-core checkouts to the frozen/refined candidate where workflows
  currently rely on the default branch; Cargo dependency pins are already exact.
- Optional docs cleanup: ngx README/design notes that still *mention* `f5f71ebc…`.
- Hardware flash and Windows UI execution when those platforms are available.
- Legacy builder/registry removal (blocked on integration, not on this audit).

No unclassified repository. No live stale pin on a classified migration
candidate.

## Appendix: commands used

Organization and default HEADs:

```bash
gh api --paginate orgs/rustscript-lang/repos --jq '.[] | [.name,.private,.archived,.fork,.default_branch,.html_url,.pushed_at] | @tsv'
gh api repos/rustscript-lang/<repo>/commits/<default_branch> --jq .sha
curl -sS 'https://api.github.com/orgs/rustscript-lang/repos?per_page=100'   # unauthenticated: 18 public, no ngx
curl -sS -o /dev/null -w '%{http_code}\n' https://api.github.com/repos/rustscript-lang/ngx-rustscript  # 404
```

Frozen core and candidate refs:

```bash
gh api repos/rustscript-lang/rustscript/commits/b1d6cffede77f49410bf63525f30b9a46b02dc01/pulls
gh api repos/rustscript-lang/<repo>/git/ref/heads/<candidate-branch> --jq .object.sha
git ls-remote https://github.com/rustscript-lang/rustscript-gpui-notepad.git 'refs/heads/subagent/gpui-consumer-migration-20636ed0'
```

Pin / no-change inspection (object reads, no live-main checkout):

```bash
git show <sha>:Cargo.toml
git show <sha>:Cargo.lock
git grep -n <pattern> <sha> -- Cargo.toml Cargo.lock .github
git ls-tree -r --name-only 0a28614f7c0024e6d8636768e898de9a689ee53b
git grep -n -i -E 'HostState|HostFunctionDescriptor|resource effect|VMBC' 0a28614f7c0024e6d8636768e898de9a689ee53b
git grep -n -i RustScript e9fe3c9f230cd9220afcd057f75702de4d7700c9
git ls-tree -r --name-only 46313ea73fa5613d80fff478a4f5414442129e40
git show 19a75f56d48f72b7818cb931b08b7490083228eb:scripts/rustscript-core-revision
```

Task 9–15 pass/fail counts come from the corresponding implementation and
read-only review summaries for the SHAs in §3. This audit did not re-run
workspace Cargo.
