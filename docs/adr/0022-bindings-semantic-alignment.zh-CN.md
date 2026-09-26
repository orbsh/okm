# ADR-0022: bindings 语义对齐——动态模式的能力上限是「有范围的」，不是「永久的」

日期：2026-09-22
状态：已接受（取代动态 codec 的「永久能力上限」裁决——PLAN Phase 8 `[~] Dynamic codec` 条目及 okm-dynamic 的 doc-comment 上限注记；ADR-0008 的事件层设计本身不受影响）。

## 背景

动态 codec 裁决（PLAN Phase 8，2026-09-12）确立了 okm-dynamic 与 Python/Steel bindings 是 schema 驱动的编解码面，并画了一条能力上限：**无 reduce、无 subscribe、无 function 索引**——语义留在 Rust 编译期，理由是「动态重建会破坏 exactly-once」。这条上限记为永久，写在 PLAN 条目和 okm-dynamic 的 doc comment 里，并被归因到 ADR-0008 的事件层框架。

那条裁决混淆了两件事。把它们分开，正是裁决要变的理由。

### ADR-0008 实际裁决的是什么

ADR-0008 裁决的是**事件层**：reduce/subscribe 是行事件流的 inline/channel 消费者，fold 钩子活在写路径的引擎锁内读-改-写里。它真正的不变量是：*fold 与文档写必须看到同一份引擎状态——累加器更新的 exactly-once 性质*。ADR-0008 从未说过 fold 函数必须是 Rust，也从未说过必须在 okm-core 内执行。ADR-0008 通篇没有提 bindings 或动态模式；okm-dynamic 上限注记里「per ADR-0008」的引用，是把 exactly-once 论证过度延伸成了对实现语言的主张。

### 裁决之后什么变了

画上限时，bindings 的使用者还是假设性的。此后出现了两种具体的部署形态，而「只编解码」的 binding 对两者都不服务：

1. **纯 Python 应用直接用**。fjall 没有 Python 绑定，裸用 KV 也繁琐——okm-dynamic 就是答案：Python 持有引擎之上的语义化 KV 门面。这个形态里 Python 侧是**唯一**写入者，不存在要互操作的「另一侧」。对这样的用户说「你的写不能维护索引条目和 reduce 组」，等于把 crate 的首要消费者挡在门外。
2. **Aura 的 Python 摊位**。摊位 产出 okm 操作、发送到 Aura 对嵌套存储执行。摊位 的语言是部署事实，不是互操作需求。

旧论证的前提是：语义需要 Rust 编译期代码，因此 bindings 侧根本不可能有语义。这个前提对函数形状的语义不成立：函数指针与语言无关——要紧的是同一语义契约被遵守，而不是哪个运行时执行它。

## 决策

**bindings 实现语义。每个语义面实现为绑定期注册的宿主语言 callable；callable 本地执行，远程路径携带的载荷是语义「结果」，绝不携带 callable。**

### 路线：宿主语言 callable，绑定期注册

每个语义面的声明侧信息已经是数据（关联常量）：`KvIndex` 的 `SLOT`/`FIELDS`/`INCLUDES`/`KEY_PREFIX`、`Reduce` 的 `SLOT`/`GROUP`，以及 `CollectionSchema` + `SlotMap` 的结构化导出。真正曾是 Rust 代码的只有语义函数本身——而这恰好是 binding 能用自己的语言提供的部分：

- **function 索引**：`Schema.add_func_index(name, fn, includes=[])`——callable 把解码后的文档映射为一个编码值或值的迭代器（多 entry fan-out，即倒排索引形态）。partial 索引的 `admits` 同形：一个谓词 callable。
- **reduce**：`Schema.add_reduce(name, group_fields, fold, unfold, acc_codec)`——callable 在宿主语言里实现 `ReduceLogic` 的 fold/unfold；累加器 codec 是声明的字节布局规则（u64 = 8 字节 BE 是种子；复合累加器走字节透明形态，与 Rust 的 `Vec<u8>` 逃生舱相同）。
- **subscribe：维持排除。** 事件发射是写路径广播，消费侧有协议（epoch 折叠、通道注册、okm-stream 组合）。它不是逐文档派生，callable 化没有收益。这一面保留上限。

### 取代上限的不变量

ADR-0008 的 exactly-once 关切由**部署形态契约**保全，而非由实现语言禁令保全：

- **嵌入模式（Python 持有引擎）**：构造上就是单写者。fold/unfold 在 binding 的 put/delete 内进程内执行，且必须精确复刻 Rust 写路径的调用纪律——put fold 新文档、delete unfold 已存文档、覆盖先 unfold 旧文档再 fold 新文档，全部共享 put 路径的引擎批次。正确性落在调用纪律上而不是语言上；调用点错了是静默的累加器漂移，所以它是验收测试的靶心。
- **远程模式（Aura：摊位 本地执行，远程机械执行）**：callable 在 摊位 自己的运行时里跑；操作载荷携带语义「结果」——派生好的条目字节、绝对累加器值（「acc 变为 42」）、以及覆盖 unfold 所需的旧文档（或其版本）。远程把它们作为一帧批量执行（ADR-0010 §2 的单次 `commit_batch`），文档写与累加器更新保持原子，而远程对两者都不做语义理解。远程不宿主 callable、不解析语义——ADR-0010 的接收方契约原样成立。

被否决的备选——把函数执行委托给远程环境、或在操作载荷里内嵌 callable——在复杂性和原则上都被否：它让远程成为外来代码的运行时，颠倒了 ADR-0010 §7 的「接收方承载引擎，不承载执行」，并把 wire 重新耦合到宿主运行时。

### 明确接受的那个代价

本地执行 reduce 跨远程链路，在一个情形下改变一致性包络：**同一 reduce group 的多写者**。Rust 原生 fold 对每个写者都是引擎锁原子；本地执行的 fold 从累加器的本地视图计算，两个写者并发 fold 同一组会丢更新。由归属规则解决：

- **每 group 单写者是远程 reduce 模式的前提。** Aura 的分区 摊位 模型按构造满足（摊位 拥有它写的数据）。同一规则下 摊位 可以本地缓存累加器——它就是权威值——稳态 put 携带绝对 acc、零额外往返；只有重启恢复时读一次 acc（与 Redis 式共享 RMW 模式的显式对照：后者需要 CAS/事务恰恰因为它多写者）。
- 多写者 group 不适用此模式。其语义执行属于拥有者进程（Rust 侧 derive，或专职的拥有者 摊位）。这是范围边界，不是要被工程化消除的缺陷。

### 语义对齐，不是互操作

实现同一声明面的 Rust 函数与 Python 函数从不跨越运行时边界：没有任何 callable 被序列化、被发送、在另一侧执行。因此「对齐」意为**对契约的语义等价**：同一声明 schema，从任一侧驱动，产出相同的条目、相同的累加器演化、相同的 unfold 补偿。验收：

1. 嵌入模式：binding 侧声明的 schema、纯 Python 驱动，语义行为符合契约——调用纪律测试（put/delete/覆盖对累加器）是核心用例。
2. 远程模式：Python 摊位 产出的操作在远端引擎执行后，与 Rust 侧产出的同一操作落得字节一致（现有跨语言字节相等测试，从 codec 字节扩展到语义条目）。

## 后果

- okm-dynamic 增加携带 callable 的面：`AccessMethod` 增加 func/admits 变体；`DynamicCollection::put`/`delete` 增加 reduce 调用纪律。字节布局的工作量小（编码器已存在）；正确性住在调用纪律里。
- okm-dynamic doc comment 与 PLAN 条目中的「永久上限」措辞修正为本 ADR 的有范围裁决：**subscribe 排除；其余在部署形态契约下皆可 callable 实现**。
- ADR-0008 不修改：其事件层设计对 Rust 原生路径成立，其 exactly-once 不变量正是本 ADR 以部署形态契约（而非实现语言禁令）重新推导的东西。
- 「暂不、非永不」：subscribe 对齐。若宿主语言的消费者故事出现（摊位 要在进程内消费 reduce 组事件），以通道协议为设计面重新开启。
