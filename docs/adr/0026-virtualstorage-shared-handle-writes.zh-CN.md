# ADR-0026: VirtualStorage 写操作取 &self — 共享句柄契约，一次说清

> **Languages:** [English](0026-virtualstorage-shared-handle-writes.md) (primary) · [中文](0026-virtualstorage-shared-handle-writes.zh-CN.md)

**Status:** Accepted (2026-09-26) — design; implementation pending, see Consequences

## Context

`VirtualStorage` 把写操作声明为 `put(&mut self)` / `del(&mut self)`
（engine/storage.rs），而读操作是 `&self`。这个 `&mut` 从来没有承担过实际职责：

1. **每个已发布的 engine 都已在内部自持同步。**
   - `FjallStore`：`#[derive(Clone)]`，注释明言契约 "Clone IS a shared handle
     (Arc-inner)"——fjall 的 `Database`/`Keyspace` 内部自持同步；impl 体调用
     的是 `&self` 方法。
   - `SlatedbSync`：字段是 `Arc<Runtime>` + `Db`；`put_sync(&self)` /
     `del_sync(&self)` 本来就存在，trait impl 只是转发。
   - `RedbStore`：包裹 `Arc<Database>`；`begin_write` 取 `&self`（redb 按设计
     就要求内部可变性）。首次写入建表经 `&self` 同样可行。
   - `TestStore` 各臂：`Arc<SlatedbSync>` 或持自身同步句柄。
   在每个 impl 里，`&mut` 接收器都被直接收窄回 `&self` 调用；它从未被用来在
   操作过程中持有独占状态。

2. **okm 内部已经存在反例 trait。** `VirtualStorageAsync`
   （slatedb_backend.rs）声明了完全相同的操作集——包括
   `async put(&self)` / `async del(&self)`。sync trait 的 `&mut` 与 async 镜像
   自相矛盾：同一份契约的两处表述，其中一处是假的。

3. **`SharedVirtualStorage` 在上一层已经陈述了真实语义。**
   `shared_handle()` 返回 "a handle to the same physical engine. Cheap;
   shares all state."。一个 clone 共享全部状态、写入却要求 `&mut` 的句柄，是
   一个只能躲在 `Arc<Mutex<>>` 后面才编译得过的契约——而这正是今天每个真实
   消费者都在付的税。

代价不是假设。aura 的 `MqStore` 只为这个签名就把 engine 包进
`Arc<Mutex<MqEngine>>`，它的 realm/ns 派生句柄还要重包内部 engine
（`Arc::new(Mutex::new(inner.lock().clone()))`），让锁边界随句柄静默漂移。
trait 上的 `&mut` 是根：它强迫消费者制造 `&mut` 路径，而数据层根本不存在
那种独占。

## Decision

1. **`put` 和 `del` 取 `&self`。** `scan_suffix`/`scan_range`/`get` 已是
   `&self`；此改动之后 `VirtualStorage` 的每个方法共享同一个接收器，trait 只
   陈述一件事：engine 句柄是共享句柄，写操作也是。engine 内部同步（fjall、
   redb、slatedb runtime）才是独占真正居住的地方。

2. **`KvBatch` 保持 `&mut`。** batch 累积真正是有状态的（一个正在构建的 op
   列表）；engine 上的 `batch()`/`commit_batch()` 同样保持 `&mut`——batch 的
   累积阶段按构造就是单一拥有者。放宽针对的是 engine 句柄，不是文件里每个
   `&mut`。

3. **不搞兼容别名、不拆 trait。** 并排一个 `VirtualStorageShared` 或用默认方法
   技巧，都会保留同一契约的两种拼法——正是本 ADR 要移除的漂移。一个 trait、
   一个接收器；所有 impl 在同一个提交里移动。

## Honest semantic cost

- 依赖 `&mut` 获得「廉价单线程访问」推理的消费者（例如没有内部同步的
  `Rc`-型 engine）不再能不加锁/不加 cell 地实现 trait。没有已发布的 engine
  属于这一类——每个都已同步——但未来想按构造即 `!Sync` 的 engine 需要在自己
  内部包 Mutex。独占要求移动到需要它的 engine 处，而不是强加给所有消费者。
- 放宽扩大了句柄允许的操作（从共享引用写入）。它不改变 engine 自身的任何运
  行时行为；内部锁原样不动。
- 下游清扫包含 okm-core 之外的每个消费者 impl：aura 的
  `MqEngine`/`MqStore`、prism 的 echo 平面适配器。它们的 `&mut` 管线
  （`let mut store = mq.clone()` 模式）在同一波机械简化。

## Consequences

- okm-core：engine/storage.rs 的 trait 签名改动；更新 8 个 impl
  （`FjallStore`、`SlatedbSync`、`RedbStore`、`TestStore`、`RemoteStore`、nest
  的 `TestEngine`、obj_dict 测试的 `Engine`，及文档注释引用）。
  `FjallStore::put`/`del` 去掉 `&mut`，转发到同样的 `&self` fjall 调用；
  `RedbStore` 同理。全测试套件运行（按 TestStore 矩阵覆盖构建携带的所有
  engine）。
- aura 在同一波消费放宽后的 trait：`MqStore` 拆掉 `Arc<Mutex<>>`，变成
  `{ prefix: Vec<u8>, engine: MqEngine }`；`for_realm`/`ns_raw` 变成纯前缀拼
  装（不再重包）。这直接删除 aura ADR-0030 的 step-1 修法——终态形状一次落
  地。按跨仓库规则 sibling（okm）先落地；aura 的 ADR-0030 记录这一顺序变更。
- 向泛型辅助函数传 `&mut S` 的消费者（`store_exec` 型接缝）收窄为 `&S`；为制
  造 `&mut` 而持有 owned 局部量的调用点随之简化。
- okm 不建 PLAN phase；这是 API 契约修正，记录于此及消费者仓库的 ADR。
