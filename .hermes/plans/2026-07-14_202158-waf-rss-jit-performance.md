# pd-edge-waf RSS + pd-vm Trace JIT Performance Implementation Plan

> **For Hermes:** Use subagent-driven-development skill to implement this plan task-by-task.

**Goal:** 在保持纯 RSS WAF 语义、matched IDs、anomaly score 与 HTTP allow/block 行为一致的前提下，消除 call-boundary micro-trace，并同时降低 RSS 规则执行的解析、转换和状态搬运成本。

**Architecture:** 采用分层方案。Core 侧先使用调用点 operand type 修正 builtin 特化，并拒绝无有效计算的 call-boundary trace；随后用 sparse dirty-local exit 降低剩余 deopt 成本。RSS 侧把规则静态字段改成 typed 参数，把 transformation 列表编译成有序整数 plan，并使用通用 literal string builtin 替换手写字符循环。每一层独立测量，未达到结构指标时不得进入下一层性能结论。

**Tech Stack:** Rust 2024、pd-vm trace JIT/Cranelift、RustScript RSS、Python CRS generator、pd-edge HTTP E2E。

---

## 1. Current baseline

当前受控 workload：`pd-edge-waf/rules/ruleset_bundle.rss`，benign GET `/products?category=books&page=2`。

| 指标 | 当前值 |
|---|---:|
| Interpreter | 约 7.527 s/request |
| Trace JIT | 约 38.939 s/request |
| JIT / Interpreter | 5.173x |
| Program locals | 238 |
| Bytecode | 19,059,623 bytes |
| Constants | 1,493,954 |
| Native traces（单请求） | 583 |
| Native executions / exits | 2,169,687 / 2,169,687 |
| Linked handoffs | 0 |
| 三-op traces | 563 |
| 三-op trace execution share | 99.986% |
| Tagged exit clones（单请求估算） | 520,653,162 |

最热形状为 `ldloc ldloc call`，对应 `engine_text.rss` 的 `while cursor < value.length`。调用点 operand metadata 为 `String`，entry local metadata 因 slot reuse 显示 `Null`，导致 `StringLen` 特化失败。

## 2. Considered approaches

### A. RSS-only

优点：可以减少规则数组、数字解析、transformation 字符串扫描和手写字符循环，Interpreter 会明显改善。

限制：JIT 仍可能为其他 generated RSS workload 生成三-op call trace，完整 locals materialization 仍存在。

### B. JIT-only

优点：修正通用 pd-vm 行为，其他大型 generated RSS 也受益。

限制：WAF 每条规则仍创建 15 字段 string array，并在请求路径反复解析 targets、operator、phase、score 和 transformation 名称。

### C. Layered core + RSS（采用）

先修正 JIT 的 trace 质量，再简化 RSS 执行 ABI，最后处理 sparse exit。该方案可区分每层收益，任何阶段均可单独回归和提交。

## 3. Acceptance criteria

### Hard correctness gates

- Interpreter 与 JIT 对 benign、invalid method、SQLi、XSS、LFI/path traversal、request header、request body probes 返回相同 decision、status、score 和 representative matched IDs。
- Chain、skipAfter、paranoia、target update 和 phase 行为保持一致。
- `rules/manifest.json` 的 CRS 结构计数保持一致。
- VMBC roundtrip、reset reuse、fuel/epoch、heap ownership 和 live entry stack 回归通过。
- 所有新增 builtin index 只追加，不重排已有 index。

### Structural performance gates

- `value.length` 热点生成 `StringLen` SSA，不再形成 `ldloc ldloc call` 三-op trace。
- call-only trace（仅 load/constant/stack shuffle 后退出到 call）不得进入 native cache。
- `native_trace_exec_count / trace_exit_count` 不再接近 1:1 的百万级循环。
- weighted materialized Tagged slots 相对当前基线下降至少 99%。
- 计时前后 attempts、recorded traces、native traces 连续不变。

### Local performance targets

使用同一机器、相同 release binary、每个 case 至少 3 轮 batch 中位数：

- RSS Interpreter 相对当前基线至少 2x 改善。
- Trace JIT 相对当前基线至少 4x 改善。
- 最终 `jit_to_interpreter_ratio <= 1.20`；理想目标 `< 1.0`。
- 共享 CI 只记录数字和结构指标，不以 wall-clock 作为 hard gate。

---

## Track A — pd-vm trace JIT

### Task A1: Put trace-shape diagnostics into the existing WAF benchmark

**Objective:** 将临时 harness 中的关键结构指标纳入可重复 benchmark 输出。

**Files:**
- Modify: `/home/wow/rustscript/pd-edge-waf/tests/perf.rs`

**Steps:**

1. 新增 `TraceShapeStats`，从 `vm.jit_snapshot()` 汇总 terminal、`has_call`、entry depth、op count、executions、SSA exits、boxed sites。
2. 输出 `native_exec`、`trace_exits`、`loop_backs`、`handoffs`、`short_trace_exec` 与 `estimated_materialized_slots`。
3. 在 measured region 前后断言结构计数不增长。
4. 保留 ignored perf test；CI 不设置时间阈值。
5. 运行：

```bash
WAF_PERF_WARMUP_BATCHES=1 \
WAF_PERF_BATCHES=2 \
WAF_PERF_BATCH_SIZE=2 \
WAF_PERF_JIT_STABLE_REQUESTS=2 \
WAF_PERF_JIT_MAX_WARMUP_REQUESTS=16 \
cargo test --release --test perf -- --ignored --nocapture
```

6. Commit：`test: report WAF JIT trace shape metrics`

### Task A2: Use call-site operand metadata for builtin specialization

**Objective:** 当 local slot metadata 已漂移时，使用 call IP 的 operand metadata 选择安全的 builtin specialization。

**Files:**
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/recorder.rs`
- Test: `/home/wow/rustscript/rustscript/tests/jit/jit_tests.rs`

**RED test:**

构造 slot reuse 程序：`local_types[container_slot] = Null`，但 `operand_types[len_call_ip] = (String, Unknown)`。断言当前 trace 为 call boundary。

**Implementation shape:**

```rust
fn refined_call_container_info(
    program: &Program,
    call_ip: usize,
    observed: ValueInfo,
) -> ValueInfo {
    let hinted = program
        .type_map
        .as_ref()
        .and_then(|map| map.operand_types.get(&call_ip))
        .map(|(lhs, _)| *lhs)
        .filter(|ty| matches!(ty, ValueType::String | ValueType::Bytes | ValueType::Array | ValueType::Map));
    hinted.map_or(observed, ValueInfo::tagged_typed)
}
```

- 仅用于选择 specialization；native unbox guard 仍验证 runtime tag。
- `len/slice/get/has/concat` 使用同一 refinement 入口。
- 不把 call-site hint 写回全局 `local_types`。

**GREEN assertions:**

- SSA 包含 `string_len`。
- 热循环 terminal 为 `LoopBack` 或包含完整循环 body。
- 结果与 interpreter 相同。
- native execution 大于零，call-boundary trace 不存在。

**Commands:**

```bash
cargo test --release --test jit_tests call_site_operand_hint -- --nocapture
cargo test --release --test jit_tests trace_jit_supports_string_len -- --nocapture
```

**Commit:** `perf: specialize JIT builtins from call-site types`

### Task A3: Reject zero-benefit call-boundary traces

**Objective:** 对无法越过第一个 call、且只包含 load/constant/stack shuffle 的 trace 保持 interpreter 执行。

**Files:**
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/recorder.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/trace.rs`
- Test: `/home/wow/rustscript/rustscript/tests/jit/jit_tests.rs`
- Test: `/home/wow/rustscript/rustscript/tests/jit/jit_nyi_edge_tests.rs`

**Policy:**

- 定义内部 `useful_native_op_count`；`ldloc/ldc/dup` 不计入有效计算。
- `terminal == BranchExit && has_call && useful_native_op_count == 0` 时返回明确 NYI reason，并 block 该 `(root_ip, stack_depth)` entry。
- 保留已有“循环中有显著 native 算术，再经过 host call”的 trace。
- 不增加 public `JitConfig` 字段。

**Tests:**

1. `ldloc ldloc call` root 不生成 native trace。
2. 带多步算术的 host-call trace 保持现有行为。
3. blocked entry 不反复增加 attempt。
4. depth-0/depth-1 key 分别 block。

**Commit:** `perf: skip zero-benefit call-boundary traces`

### Task A4: Core checkpoint benchmark

**Objective:** 在更改 exit ABI 前确认 A2/A3 对 WAF 的独立收益。

**Run:**

```bash
cargo test --release --test jit_tests
cargo test --release --lib vm::jit::
cargo clippy --all-targets --all-features -- -D warnings
```

随后在 `pd-edge-waf` 运行 smoke、E2E 和 perf。

**Go/no-go:**

- 若 `jit_to_interpreter_ratio <= 1.20` 且 materialized slots 已下降 99%，A5 可延后为独立 core 优化。
- 若 call exits 仍是主要成本，继续 A5。

### Task A5: Sparse dirty-local exit materialization

**Objective:** exit 只写回 trace 内被 `stloc` 修改的 locals；operand stack 仍完整恢复。

**Files:**
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/ir.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/recorder.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/liveness.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/native/lower.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/native/bridge.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/native/mod.rs`
- Test: `/home/wow/rustscript/rustscript/tests/jit/jit_tests.rs`

**IR change:**

```rust
pub struct SsaLocalMaterialization {
    pub index: usize,
    pub value: SsaMaterialization,
}

pub struct SsaExit {
    pub stack: Vec<SsaMaterialization>,
    pub dirty_locals: Vec<SsaLocalMaterialization>,
    // existing fields...
}
```

**Recorder rules:**

- `SymbolicFrame` 持有 dirty-local bitset。
- `stloc` 标记对应 slot。
- unchanged entry locals 保留在 VM Vec 中，不进入 exit buffer。
- control-flow merge 对 dirty set 取 union。

**Native restore:**

- 新增 JIT 专用 sparse restore helper，参数为 `indices + values + count`。
- index 使用 `usize/u32`，兼容 wide local slots。
- 每个 dirty slot 仍经 `store_local_with_drop_contract`，保持 drop/move 语义。
- AOT 现有 full restore ABI 保持不变。

**Tests:**

- read-only call exit：dirty locals 为 0。
- 单个 int/bool/string/map local 修改后恢复。
- 多分支 dirty-set union。
- heap value 引用计数与 reset reuse。
- guard deopt、fuel/epoch yield、linked trace handoff。
- wide local index >255。

**Commit:** `perf: restore only dirty locals on JIT exits`

### Task A6: Add append-only literal string builtins

**Objective:** 为 RSS 提供 O(n) 通用字符串原语，并允许 trace 内直接执行，避免手写 immutable concat 循环。

**Files:**
- Modify: `/home/wow/rustscript/rustscript/src/builtins/runtime/core.rs`
- Modify: `/home/wow/rustscript/rustscript/src/builtins/runtime/mod.rs`
- Modify: `/home/wow/rustscript/rustscript/src/lib.rs`
- Modify: `/home/wow/rustscript/rustscript/src/compiler/typing/helpers.rs`
- Modify: `/home/wow/rustscript/rustscript/src/compiler/typing/context.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/ir.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/recorder.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/jit/native/lower.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/native/bridge.rs`
- Modify: `/home/wow/rustscript/rustscript/src/vm/native/mod.rs`
- Test: `/home/wow/rustscript/rustscript/tests/jit/jit_tests.rs`
- Test: `/home/wow/rustscript/rustscript/tests/wire/wire_tests.rs`

**Builtins:**

- `string_contains(text, needle) -> bool`
- `string_replace_literal(text, needle, replacement) -> string`
- `string_lower_ascii(text) -> string`

**Constraints:**

- builtin enum/index 只追加。
- `lower_ascii` 与现有 RSS `lower()` 的 A-Z 行为一致。
- empty needle、UTF-8、replacement 自引用、no-match 保持明确语义。
- JIT 使用 specialized SSA/helper，不经 interpreter call boundary。
- AOT 与 interpreter 返回完全一致。

**Commit:** `feat: add native literal string builtins`

---

## Track B — pd-edge-waf RSS generation and engine

### Task B1: Replace the 15-string rule array with a typed rule ABI

**Objective:** 消除每条规则的 array allocation/indexing，以及 static integer/bool 字段的 `number()` 解析。

**Files:**
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine.rss`
- Modify: `/home/wow/rustscript/pd-edge-waf/tools/convert_crs.py`
- Regenerate: `/home/wow/rustscript/pd-edge-waf/rules/*.rss`
- Regenerate: `/home/wow/rustscript/pd-edge-waf/rules/ruleset_bundle.rss`
- Regenerate: `/home/wow/rustscript/pd-edge-waf/rules/pd_edge_waf.rss`
- Test: `/home/wow/rustscript/pd-edge-waf/tests/smoke.rs`

**Target API shape:**

```rss
pub fn apply_rule(
    state: map<string>,
    id: int,
    phase: int,
    chain_index: int,
    has_chain: bool,
    targets: string,
    operator: string,
    pattern: string,
    transform_plan: int,
    paranoia: int,
    score: int,
    disruptive: bool,
    status: int,
    skip_after: string,
    message: string
) -> map<string>
```

Generator 输出 typed literal，不再生成 `["R", "942100", ...]`。

**Tests:**

- converter unit tests覆盖 typed literals、negated operator、chain child、marker、target update。
- generated tree deterministic。
- manifest 数字不变。
- smoke/E2E decision 不变。

**Commit:** `perf: generate typed RSS rule calls`

### Task B2: Compile ordered transformations into an i64 plan

**Objective:** 移除请求路径中的 transformation 名称字符串扫描，并严格保留 CRS transformation 顺序。

当前 CRS 数据：437 条规则有 transformation，45 个唯一 pipeline，最大长度 8。使用 6-bit opcode 可在 i64 中保存完整有序序列。

**Files:**
- Modify: `/home/wow/rustscript/pd-edge-waf/tools/convert_crs.py`
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine_text.rss`
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine.rss`
- Add tests: `/home/wow/rustscript/pd-edge-waf/tests/transformations.rs`

**Encoding:**

- opcode 0 为结束；其余 transformation 使用 append-only 1..63。
- 第一个 transformation 放最低 6 bit，循环执行后右移 6 bit。
- `none` 编译期移除。
- 未实现 transformation 在生成期明确报错或进入 manifest audit；不得静默忽略。

**Test matrix:**

- 45 个唯一 pipeline 全部生成 golden plan。
- order-sensitive pairs：`lowercase,urlDecodeUni` 与 `urlDecodeUni,lowercase` 分别验证。
- 最大 8-op pipeline。
- benign/SQLi/XSS/LFI probes 保持 matched IDs 和 score。

**Commit:** `perf: compile CRS transformations into ordered plans`

### Task B3: Replace manual text loops with native literal primitives

**Objective:** 删除 `contains/replace/lower` 的逐字符 immutable concat 热路径。

**Files:**
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine_text.rss`
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine_operators.rss`
- Regenerate bundles with `/home/wow/rustscript/pd-edge-waf/tools/bundle_engine.py`
- Test: `/home/wow/rustscript/pd-edge-waf/tests/transformations.rs`

**Mapping:**

- `contains` → `string_contains`
- `replace` → `string_replace_literal`
- `lower` → `string_lower_ascii`
- `urlDecodeUni/htmlEntityDecode/normalizePath/cmdLine` 仍由 RSS 决定转换语义，只调用通用 literal primitive。
- regex operator 继续使用 `re::*`；literal replacement 不经 regex。

**Commit:** `perf: use native literal string primitives in RSS WAF`

### Task B4: Specialize static target/operator dispatch only if still measured hot

**Objective:** 根据 B3 后 diagnostics 决定是否继续消除 `ctx_targets()` 的 `re::split("\\|")` 和 operator-name chain。

**Files if needed:**
- Modify: `/home/wow/rustscript/pd-edge-waf/tools/convert_crs.py`
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine_context.rss`
- Modify: `/home/wow/rustscript/pd-edge-waf/rules/engine_operators.rss`

**Preferred design:**

- generator 输出 compact target opcode plan，保留 selector 常量。
- operator 使用 typed opcode，而非运行时比较 `"@rx"/"@pm"/...`。
- exclusion、TX macro expansion、chain 和 skipAfter 顺序保持现状。

**Go/no-go:** 仅当 profile 显示 target/operator parsing 仍占主要时间时实施。避免在 string/JIT 主因解决前扩大生成器复杂度。

**Commit if needed:** `perf: compile WAF target and operator dispatch`

---

## Integration and release sequence

### Task C1: Full correctness verification

Core：

```bash
cargo fmt --all -- --check
git diff --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release --test jit_tests
cargo test --release --lib vm::jit::
cargo test --release --test wire_tests
```

WAF：

```bash
bash tools/smoke.sh
python3 tools/bundle_engine.py
cargo fmt --all -- --check
git diff --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --release --test smoke
cargo test --release --test e2e -- --nocapture
```

### Task C2: Controlled before/after benchmark

- 使用同一 release binary 和相同请求。
- compile 在计时外。
- JIT 至少经过 `hot_loop_threshold` 个完整请求，并连续两次 compilation state 不变。
- 运行 3 轮完整 batch，报告 median、min/max batch average。
- 同时报告结构指标；wall-clock 受主机负载影响时，以结构指标和同 binary 重跑为主。

输出至少包含：

```text
baseline_average_us
interpreter_average_us
jit_average_us
jit_to_interpreter_ratio
attempts / traces / native_traces
native_exec / trace_exits / loop_backs / handoffs
short_trace_exec
estimated_materialized_slots
bytecode_bytes / constants / native_code_bytes
```

### Task C3: Review and publish

1. Core spec review：call-site hints、guard/deopt、sparse ownership、fuel/epoch、wide local。
2. Core quality review后提交并推送；确认 exact-SHA CI success。
3. WAF 更新到已发布 core revision。
4. WAF smoke/E2E/perf 复跑。
5. WAF 提交并推送；确认 exact-SHA CI success。

---

## Risks and tradeoffs

- **Call-site hint correctness:** hint 只决定 specialization 候选，runtime unbox guard 仍是安全边界。
- **Sparse exit ownership:** unchanged VM locals 必须保持有效；dirty heap local 的 clone/drop 顺序需要专门测试。
- **Transformation parity:** 当前 engine 对部分 transformation 仅做近似处理；plan 编码必须保留当前行为，并把未实现项显式列出。
- **Builtin compatibility:** append-only index、VMBC roundtrip、AOT/interpreter/JIT 三路径必须同时覆盖。
- **Generated code size:** typed ABI 会减少数组和解析，compiler inlining 仍可能复制 helper；每阶段记录 bytecode/native code bytes。
- **Shared host variance:** CI 不使用绝对时间 gate。

## Deferred scope

以下内容不进入首轮：

- 通用 RSS user-function call frame / no-inline compiler架构；
- 跨请求 transformation result cache；
- CRS domain-specific builtin；
- 全量 AOT WAF 重构；
- 以 generic runtime rule table 替换直接 generated evaluator。

只有完成 A2/A3/B1/B2/B3 后，新的 profile 仍指向 function inlining 或 target parsing，才开启后续设计。
