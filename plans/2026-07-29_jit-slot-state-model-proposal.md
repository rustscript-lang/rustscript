# Trace JIT slot-state 模型提案

> 范围：阶段 5 的设计产物。本文不修改 deopt、region fusion 或 side-link 的运行时实现。

## 目标与非目标

目标是建立一个能检验恢复路径正确性的状态模型，并明确哪些数据是权威来源。它只复用底层概念，暂不合并 `SsaExit`、`SideTraceImport` 与 `VirtualFrameSnapshot` 的类型。

非目标：

- 不以减少类型数为目标；
- 不将 owned 与 borrowed 值归并为一个无约束字段；
- 不改变当前 direct-link、W^X、keepalive 或 atomic slot 发布协议；
- 不以新增恢复分支交换较少的数据结构。

## 基本模型

```text
SlotId = FrameId × SlotKind × SlotIndex
SlotKind = Operand | Local | Capture

StateCell = {
  slot: SlotId,
  materialization: Materialization,
  ownership: OwnershipMode,
  dirty: bool,
}

OwnershipMode = Borrowed | Owned | Scalar
Materialization = existing SsaMaterialization
```

`SlotId` 是恢复计划的地址，不是值本身。`Materialization` 保持现有 SSA 语义；任何未来的通用模型都必须保留其具体变体。`OwnershipMode` 用于校验转移是否合法，不能从 `ValueType` 推断。

## 权威状态来源

| 场景 | operand stack / locals 的权威来源 | capture cell 的权威来源 | 允许写入者 |
|---|---|---|---|
| 物理 frame 正在解释执行 | `Vm` 物理 stack 与 locals | `CallableEnvironment` | interpreter / 已完成的 native 恢复 |
| native trace 内 | SSA value + dirty 集合 | 已获准借用的 environment | native lowering 产生的显式 store |
| native exit | `SsaExit` materializations 与 dirty 集合 | 现有 capture cell | sparse restore helper |
| virtual frame | `VirtualFrameSnapshot` 的完整 materializations | 关联 callable environment | virtual-frame restore helper |
| parent exit → side entry | `SideTraceImport` 的经验证 mapping | parent/child 共享关系 | side-entry materialization |
| fused region | remap 后 SSA ID 与 link metadata | 原 callable environment | region lowering |

物理 frame 的 clean slot 必须继续从 `Vm` 读取；dirty slot 必须由当前 SSA materialization 覆盖。virtual frame 没有可回退的物理 locals，故必须完整构造。

## 五类状态转换

### 1. 物理 frame 的稀疏恢复

输入为 `SsaExit`、物理 frame 标识与 dirty cells。对每个 dirty cell：先计算 source，后根据 ownership 选择 replace、move 或 scalar store。clean locals 与 clean operand stack 保持 `Vm` 中的当前内容。

前置条件：exit entry shape 与物理 frame 的 stack/local 边界一致。后置条件：每个 dirty `SlotId` 与 interpreter 在相同 bytecode IP 的可观察值相同。

### 2. virtual frame 的完整构造

输入为 `VirtualFrameSnapshot`。它必须为每个 operand 与 local 提供 materialization；不得把缺失条目解释为“沿用物理 frame”。构造顺序按 frame 链从 caller 到 callee；每个 callable environment 在恢复前完成身份校验。

### 3. parent exit 到 side entry

`SideTraceImport` 是带验证的表示转换，不是 copy 的快捷路径。它只能导入 parent exit 的显式 inputs，按 child entry 参数顺序建立 mapping；frame、stack 深度、local 数、SSA repr 与 ownership 任一不匹配时拒绝 admission。

### 4. dirty-local 传播

dirty 集合是写入事实的权威来源。记录器在 store/可观察 mutation 后添加 cell；控制流 merge 取并集；region fusion 在 ID remap 后保持同一 slot 语义。exit lowering 只能恢复 dirty cells，不能依赖“值看似相同”省略所有权转移。

### 5. borrowed 的并行赋值

同一恢复批次先计算全部 source，再写 destination。borrowed heap source 在任何 destination overwrite 前完成保留或 clone；owned source 在成功转移后只清理一次。swap、cycle、重复 borrowed source 都不得依赖目标迭代顺序。

## 不变量

1. 一个可观察的 owned heap value 在任意时刻只有一个销毁责任方。
2. 同一 borrowed source 可写入多个 destination，且每个 destination 的 retain/clone 语义独立。
3. 任何失败 helper 在可观察写入之前返回；已完成的 writes 必须有明确的重放或回滚契约。
4. `SlotId` 的 frame 域在 region fusion 后仍唯一；SSA ID 重映射不能改变 frame/slot 身份。
5. side entry 不得绕过 parent exit 的 repr、ownership、frame 与 generation 校验。
6. virtual frame 的每个 slot 都有 materialization；物理 frame 只允许 clean-slot 回退。

## 必测矩阵

| 情形 | 断言 |
|---|---|
| dirty source 与 dirty destination | source 与 destination 均按其 materialization 恢复 |
| 正向与反向索引 | 结果不依赖 destination 遍历顺序 |
| swap / cycle | 不丢失任一 heap 值，drop 次数正确 |
| 重复 borrowed heap value | 每个目标仍可独立存活 |
| virtual frame clean local | 完整 materialization，不读取 caller 的物理 local |
| nested virtual frame | frame 链、return IP 与 callable 身份一致 |
| region remap ID 碰撞 | 不混淆不同 frame 的 `SlotId` |
| observable single-owner drop | 销毁一次且仅一次 |
| helper/instruction failure replay | 恢复后与 interpreter 的重试行为一致 |
| side import repr/ownership mismatch | admission 拒绝且不发布 native link |

## 原型与决策门

后续原型只可先引入内部 `SlotStatePlan` 视图，并由现有 `SsaExit`、`SideTraceImport`、`VirtualFrameSnapshot` 生成；三种现有类型保留为外层契约。原型需证明：

- 上述测试矩阵全部通过；
- 恢复路径分支数不增加；
- helper failure、drop 与 side-entry admission 的诊断信息未减弱；
- JIT 热路径没有显著性能退化。

若模型仅减少类型名称或字段数，却增加恢复分支、模糊所有权或使任何不变量无法局部验证，则停止，不进入实现阶段。
