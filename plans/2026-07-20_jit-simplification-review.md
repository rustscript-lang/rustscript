# RustScript Trace JIT 简化实施计划（修订版）

> **For Hermes:** 本计划按独立阶段实施；每阶段单独验证、评审和提交，不跨阶段混合改动。

> **性质：** 架构简化计划。本次更新仅修改本计划文件，不修改源码。

**目标：** 降低 `pd-vm` trace JIT 的维护复杂度，使新增 builtin 特化通常只需修改 1–2 个权威入口，同时保持 interpreter、JIT、AOT、wasm/no-std 语义一致。

**架构原则：** 先移除孤立源码，再用声明式元数据生成机械接线；保留 typed IR、所有权验证和专用 lowering。deopt 与机器码链接属于后续架构决策，必须先建立状态模型和实测依据。

**技术栈：** Rust 2024、Cranelift 0.129、trace SSA、AOT SSA、Cargo feature matrix。

**仓库基线：** `/home/wow/rustscript/rustscript`，`master @ fd3ddc9f747eae821578cbc74447d174b9fdbbbb`。

---

## 一、复核后的代码规模

### 1.1 递归目录总量

| 范围 | 行数 | 备注 |
|---|---:|---|
| `src/vm/jit/**/*.rs` | 19,509 | 包含 `jit/native/mod.rs` 与 `jit/mod.rs` |
| `src/vm/native/*.rs` | 7,659 | 包含孤立的 `native/inline.rs` |
| **递归合计** | **27,168** | 原总数正确 |
| `src/vm/native/inline.rs` | 3,613 | 当前未进入模块树 |
| **当前活跃 JIT/native 源码** | **23,555** | 递归合计减去孤立文件 |
| `src/vm/aot/**/*.rs` | 8,835 | 独立 AOT 后端 |
| JIT 测试 | 9,677 | 含测试入口文件 |

原表漏列了以下 610 行，但总计已包含：

| 文件 | 行数 |
|---|---:|
| `src/vm/jit/native/mod.rs` | 446 |
| `src/vm/jit/mod.rs` | 16 |
| `src/vm/native/mod.rs` | 76 |
| `src/vm/native/offsets.rs` | 72 |

### 1.2 结构计数

| 指标 | 当前值 |
|---|---:|
| `SsaInstKind` | 77 个变体 |
| `SpecializedBuiltinKind` | 36 个变体 |
| `NativeInlineStep` | 46 个变体 |
| `bridge.rs` `extern "C"` helper | 50 个 |
| `lower.rs` 中 `SsaInstKind::` 引用 | 209 处 |
| `lower.rs` 函数 | 112 个 |

### 1.3 LuaJIT 对照的使用边界

LuaJIT 单后端 JIT 核心约 15k 行，可作为复杂度参照，但不作为 RustScript 的硬目标：

- LuaJIT 也维护 interpreter、JIT、多 CPU 后端、C function 与 int/num IR；
- RustScript 额外维护 AOT、wasm/no-std、host pending/yield、fuel/epoch、所有权与多种 Value 表示；
- LuaJIT 可直接修补自己生成的机器码；Cranelift finalized code 没有同等公开链接接口；
- 行数比较只能说明 RustScript 有明显机械重复，不能单独证明某个抽象应删除。

**修订结论：** 以“新增 builtin 的权威修改点数量”和“重复语义数量”为主要指标；LOC 仅作趋势数据。

---

## 二、复核后的复杂度判断

### 2.1 builtin 特化存在结构性重复

一个 builtin 可能涉及：

1. `src/vm/jit/ir.rs`：typed IR、inputs、render；
2. `src/vm/jit/recorder.rs`：选择、分析、发射；
3. `src/vm/native/bridge.rs`：fallible 或 owned helper；
4. `src/vm/native/codegen.rs`：Cranelift signature；
5. `src/vm/native/mod.rs`：地址导出；
6. `src/vm/jit/native/lower.rs`：临时槽、所有权、failure exit、helper 调用。

其中选择、arity、输入表示、输出表示、helper ABI 等信息适合声明式生成；所有权转移、别名处理、failure exit 和特殊控制流仍需 typed 逻辑。

### 2.2 活跃 native 机制应按“两套 + 一份孤立源码”描述

| 部分 | 状态 |
|---|---|
| trace SSA lowering | 活跃 |
| AOT SSA | 活跃且独立 |
| `src/vm/native/inline.rs` | 未被 `mod` 声明，全仓库没有外部消费者 |

因此 `native/inline.rs` 当前不是第三套运行机制，可以优先删除；无需再猜测 wasm/no-std 是否复用。

### 2.3 deopt 的复杂度确实高，但各结构语义方向不同

- `SsaExit`：父 trace 离开点的恢复描述；
- `SideTraceImport`：子 trace 入口的值映射和表示适配；
- `VirtualFrameSnapshot`：尚未存在的 callee frame，要求完整 materialization；
- `dirty_locals`：已存在物理 frame 的稀疏覆盖集合；
- region dirty propagation：融合图中的跨边状态传播。

它们可以共享底层 slot schema，但不能直接合并为同一个列表后删除其余语义。

### 2.4 tail wrapper 可以共享构建逻辑，运行时数量未必能减到两个

当前 wrapper 涉及普通、inherited、owned、dispatcher 等不同 ABI 与清理契约。第一目标应是参数化生成与消除重复源码；是否能减少运行时 thunk 数量，需要 ABI 矩阵和性能数据证明。

### 2.5 诊断 API 已形成公共契约

`jit_native_*`、`jit_snapshot()`、`dump_jit_info()` 被测试、example 和外部调用使用。默认关闭 feature 会移除方法或改变输出，属于兼容性变更，不能归类为无风险内部整理。

---

## 三、修订后的执行顺序

## 阶段 0：删除孤立的 `native/inline.rs`

**目标：** 先移除已确认没有消费者的 3,613 行源码。

**文件：**

- Delete: `src/vm/native/inline.rs`
- Verify: `src/vm/native/mod.rs`
- Verify: `src/vm/jit/native/mod.rs`

**步骤：**

1. 在删除前再次搜索以下符号，确认只有文件内部定义和引用：
   - `NativeInlineStep`
   - `InlineEmitCtx`
   - `emit_native_inline_step`
2. 确认 `src/vm/native/mod.rs` 没有 `mod inline;`。
3. 删除文件。
4. 运行格式、workspace tests、strict Clippy、文档和 AArch64 check/Clippy。
5. 分别检查默认 feature、`--no-default-features`、wasm/no-std workspace 成员。
6. 独立评审删除范围，确认无构建脚本或源码生成器按路径读取该文件。

**验收：**

- 所有门禁通过；
- 活跃行为和生成产物不变；
- JIT/native 递归总量下降 3,613 行。

**风险：** 低。主要风险是非 Rust 模块消费者按文件路径读取源码，因此必须搜索 build script、脚本和文档引用。

---

## 阶段 1：声明式 builtin 元数据试点

**目标：** 先证明一个元数据模型能覆盖三类差异明显的操作，再推广到全部 builtin。

**试点操作：**

| 操作 | 代表语义 |
|---|---|
| `StringLen` | 纯读取、标量结果 |
| `RegexMatch` | fallible helper、failure exit、bridge error |
| `ArraySet` | owned mutation、别名、源值保留、failure exit |

### 1.1 设计权威元数据

**候选文件：**

- Create: `src/vm/jit/builtin_spec.rs`
- Modify: `src/vm/jit/mod.rs`
- Modify: `src/vm/jit/recorder.rs`
- Modify: `src/vm/jit/ir.rs`
- Modify: `src/vm/jit/native/lower.rs`
- Modify: `src/vm/native/codegen.rs`
- Modify: `src/vm/native/mod.rs`

元数据至少表达：

- builtin identity 与 arity；
- 输入数量、runtime representation 和静态类型要求；
- 输出 representation、known type、是否拥有临时槽；
- effect：pure / fallible / owned mutation / iterator state；
- failure exit 要求；
- bridge ABI class；
- mutation 输入中哪些值可能与容器别名；
- 成功和失败时的所有权契约。

### 1.2 采用生成 typed glue 的宏，不采用运行时字符串分派

建议形态：

- 宏或 const spec 生成 builtin 选择和基础分析；
- 生成 signature/address registry；
- 继续保留 typed `SsaInstKind`；
- 特殊 lowering 仍由明确函数实现；
- helper 地址使用函数项或强类型 ID，不使用 `&'static str`；
- 不引入运行时统一 opcode dispatch。

### 1.3 试点验证

每个试点必须验证：

- interpreter/JIT/AOT 结果一致；
- SSA dump 的语义结构一致；
- fuel/epoch 计数不因生成层增加 synthetic op；
- helper 失败恢复 virtual frame 并只重放一次；
- mutation helper 在源/目标/参数别名下 clone-before-transfer；
- bridge TLS 在 replay 前清理；
- native trace 数量、代码字节、编译时间和 benchmark 无显著退化。

### 1.4 试点决策门

只有满足以下条件才推广：

- 新增同类 builtin 通常只改 spec + 专用语义实现；
- 所有权和 failure exit 信息不再散落到弱类型字符串；
- generated code 可读、错误位置可定位；
- compile time 和运行性能无显著退化；
- 三类试点均无需为自身增加例外旁路。

**初始收益目标：** 减少修改点和重复 match；不预设净删 4–6k 行。

---

## 阶段 2：推广 builtin 元数据并建立复杂度报告

**目标：** 将机械部分逐族迁移，避免一次重写 36 类特化。

**建议迁移顺序：**

1. len/type/predicate 类；
2. string/bytes 纯转换；
3. regex 和其他 fallible helper；
4. array/map 查询；
5. array/map mutation；
6. iterator state。

每一族独立提交，并运行该族定向测试与完整 JIT tests。

**复杂度报告指标：**

- 新 builtin 的权威修改点数量；
- 重复 selection/analyze/emit 分支数量；
- helper signature/address 重复数量；
- typed IR 变体数量；
- JIT/native LOC 趋势；
- native compile latency、代码字节和执行 benchmark。

LOC 只报告，不设置阻断阈值。

---

## 阶段 3：诊断实现整理，保持公共 API

**目标：** 整理 `runtime.rs` 的诊断实现，不直接移除公开方法。

**候选方案：**

- 将 snapshot/dump/metrics 组装移动到 `src/vm/jit/diagnostics.rs`；
- `Vm` 上的公开方法保留为薄转发；
- 只对高成本采集项目设置内部开关；
- 如果需要默认关闭公开诊断能力，先写兼容性提案并确定版本策略；
- feature 关闭时的返回语义必须明确，不能静默伪造零值。

**验收：**

- 现有调用方无需改动；
- dump 和 snapshot 内容保持兼容；
- 默认热路径没有新增分支或分配；
- 不承诺固定删减行数。

---

## 阶段 4：tail wrapper ABI 分类与参数化生成

**目标：** 减少重复 Cranelift 构建代码，先不承诺运行时只剩两个 wrapper。

**步骤：**

1. 建立 ABI/语义矩阵：
   - 参数；
   - 返回 status；
   - inherited state；
   - owned slot 清理；
   - direct-link slot；
   - dispatcher trace ID；
   - W^X 与 keepalive 生命周期。
2. 识别可共享的 block builder、status tail 和 owned cleanup 片段。
3. 参数化源码生成，同时保持不同 ABI 的独立入口。
4. 用反汇编、代码字节和 tail-link benchmark 比较前后结果。
5. 只有 ABI 完全一致且没有额外间接跳转时，才合并运行时 wrapper。

**验收：** direct link、inherited handoff、owned clear、fuel/epoch、失效重发布测试全部通过。

---

## 阶段 5：统一 slot-state 模型的设计提案

**目标：** 先建立形式化状态模型，暂不改 deopt 实现。

提案必须分别定义：

- 物理 frame 的稀疏恢复；
- virtual frame 的完整构造；
- parent exit 到 side entry 的表示转换；
- dirty-local 传播；
- borrowed source 的并行赋值语义；
- region fusion 中 SSA ID remap；
- clean/dirty local、operand stack 和 capture cell 的权威来源。

可以共享的底层结构是 `SlotId + Materialization + OwnershipMode`；是否合并 `SsaExit`、`SideTraceImport`、`VirtualFrameSnapshot`，由模型证明和原型结果决定。

**必测矩阵：**

- dirty source 与 dirty destination；
- 两种索引顺序；
- swap/cycle；
- 重复 borrowed heap value；
- virtual frame clean local；
- nested virtual frame；
- region remap ID 碰撞；
- observable single-owner drop；
- helper/instruction failure replay。

**决策门：** 若模型只减少类型数量，却增加恢复分支或弱化所有权证明，则停止该阶段。

---

## 阶段 6：dynasm-rs 链接 spike（可选）

**事实边界：** dynasm-rs 可以修补其自己管理的 executable buffer，不能直接修改 Cranelift 已 finalized 的代码。

spike 只评估三种明确方案：

1. 保留现有 atomic link slot；
2. 使用 dynasm-rs 生成可修补 entry/exit stub，Cranelift trace 跳到 stub；
3. 用 dynasm-rs 或自有后端生成完整 trace。

方案 2 仍可能保留间接层；方案 3 接近新增完整后端。不得预设引入 dynasm-rs 后即可删除 region fusion 和全部 wrapper。

**评估指标：**

- hot side-link 执行延迟；
- trace 编译延迟；
- 机器码字节；
- x86_64/AArch64 覆盖；
- W^X、并发 patch、指令缓存同步；
- unwind/debug/反汇编支持；
- 维护代码量和测试矩阵。

只有端到端收益显著且维护成本可接受，才进入正式设计。

---

## 四、阶段顺序与风险

| 顺序 | 阶段 | 预期收益 | 风险 | 是否立即实施 |
|---:|---|---|---|---|
| 0 | 删除孤立 `native/inline.rs` | 明确减少 3,613 行 | 低 | 是 |
| 1 | 三类 builtin 元数据试点 | 验证架构 | 中 | 设计评审后 |
| 2 | 按族推广 + 复杂度报告 | 降低新增特化成本 | 中 | 试点通过后 |
| 3 | 诊断实现整理 | 文件职责更清晰 | 低至中 | 保持 API 前提下 |
| 4 | wrapper 参数化 | 减少重复生成逻辑 | 中 | builtin 整理后 |
| 5 | slot-state/deopt 提案 | 降低推理成本 | 高 | 暂缓实现 |
| 6 | dynasm-rs spike | 评估直接 patch 路径 | 高 | 可选 |

每阶段独立 PR，不合并提交。阶段 0–2 完成后重新统计活跃代码和修改点，再决定阶段 4–6 是否值得继续。

---

## 五、验收标准

### 核心目标

- 新增普通 builtin 特化通常只需修改 1–2 个权威入口；
- typed IR、所有权和 failure exit 契约保持显式；
- interpreter/JIT/AOT/wasm/no-std 语义一致；
- native 性能无显著退化；
- 复杂度减少来自删除重复语义，不来自压缩格式或弱化检查。

### 通用验证

- `cargo fmt --all -- --check`
- `cargo test --workspace --all-features`
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- `cargo doc --workspace --all-features --no-deps`
- AArch64 workspace check 与 Clippy
- 默认 feature 与 `--no-default-features` 构建
- JIT 定向 ownership/deopt/identity/schema/region/direct-link tests
- 现有性能基准 A/B

### 评审要求

每阶段至少覆盖：

- 语义一致性；
- identity 与失效；
- ownership/drop；
- virtual-frame deopt；
- helper failure replay；
- ABI 与链接生命周期；
- feature/API 兼容性。

---

## 六、已解决和开放问题

### 已解决

1. `native/inline.rs` 的消费者：当前没有消费者，文件未进入模块树。
2. 递归总量 27,168 行：总数正确，原表漏列 610 行。
3. 诊断 feature：默认关闭会影响公共 API，不能作为无风险阶段。

### 开放问题

1. 是否接受用宏/生成 typed glue 作为 builtin spec 的主要实现，而非运行时通用表？
2. `SsaInstKind` 是否继续保持每个语义操作独立变体，还是为同 ABI 的 helper-backed 操作引入受约束的通用变体？
3. 公开诊断 API 的长期兼容策略是什么？
4. wrapper 参数化后，是否仍有足够收益继续减少运行时 thunk 数量？
5. 阶段 1–2 完成后，deopt 复杂度是否仍是最高维护成本？
6. dynasm-rs 的目标是 patchable stub，还是完整 trace 后端？

---

## 七、当前建议

先执行阶段 0，并单独评审；随后只做阶段 1 的三类试点。试点通过后再确定完整迁移范围。

暂不承诺将活跃源码降到 15–18k 行。当前最重要的成功指标是：**新增 builtin 特化的权威修改点降到 1–2 个，同时保留完整语义和所有权证明。**
