# ADR-0027: VirtualStorage 接口审计——派生便利方法退出 trait

> **语言：** [English](0027-virtualstorage-surface-audit.md)（主） · [中文](0027-virtualstorage-surface-audit.zh-CN.md)

**状态：** 已接受（2026-09-26）——同一批实施完毕（测试全绿：34 个套件，
`test-engines,arrow,parquet`）。

## 背景

mudra 全栈重写（mudra 仓 `docs/ADR-rust-fullstack.md`）让面板成为
`VirtualStorageAsync` 的第一个生产消费者，这逼出了一个问题：引擎 trait 的
接口面是否最小？下文的调用点计数是 2026-09-26 对 okm + aura + probe 的
grep 实测，不是估计。

同步 `VirtualStorage` 原有 9 个方法；异步镜像 5 个。三个嫌疑对象：

1. `scan_suffix_kv`——trait 方法，默认实现由 `scan_suffix` + `get` 派生
   （N+1 次点查）。5 个调用点全在 okm 模型层内部。从未有引擎覆写过它。
2. `batch()` / `commit_batch()` 成对存在，背后还有一个 `KvBatch` trait。
3. `shared_handle()` 独立挂在子 trait 上——审计结论：形态正确，保留。

## 决定

1. **`scan_suffix_kv` 退出 trait**，改为自由泛型函数。函数体现在由
   `scan_range_iter` 派生——一趟产出 (suffix, value)，严格优于旧的 N+1
   默认实现。没有覆写点可言：`scan_range_iter` 本来就是引擎的原生带值
   对扫描。
2. **`batch()` 退出 trait；`commit_batch(ops)` 保留。** 提交点必须持有
   引擎：fjall 把 op 列表执行进它自己的 `Batch`，redb 执行进 ONE 个写事务
   （两者都覆写 `commit_batch`——原子性是真实的）。`batch()` 返回**具体
   类型** `MemBatch`，所以任何引擎都不可能包装别的 carrier——它那三个
   "覆写"（fjall / redb / RemoteStore）全是照抄 trait 默认实现的一行体。
   fjall 旧注释（"batch() returns MemBatch for encoding; commit replays
   into fjall's own Batch"）对 commit 的描述是诚实的，却把 `batch()` 卖成
   一个它永远成不了的扩展点。
3. **`KvBatch` trait 删除；`MemBatch` 保留 inherent `put`/`del`。**
   证据：唯一实现，实现的是自己那个默认 carrier；`KvBatch::commit`
   调用点为**零**（全树的 `.commit()` 命中都是引擎内部——fjall 的
   `wb.commit()`、redb 的 `txn.commit()`）；`save_into(&mut impl
   KvBatch)` 永远不可能收到非 `MemBatch`。删除后累积 API 完全不变——
   inherent 方法，调用面零改动。
4. **`VirtualStorageAsync` 补齐对齐面。** 按**审计前**的同步 trait 计数缺
   4 个方法（`scan_range_iter`、`scan_suffix_kv`、`batch`、
   `commit_batch`）；按**审计后**要镜像的同步面计数缺 3 个
   （`scan_range_iter`、`scan_suffix_kv`、`commit_batch`）——落地形态：
   异步 `scan_range_iter`（缓冲默认 = 同步 trait 自身默认的可信镜像；
   SlatedbStore 用一趟原生 `DbIterator` 物化覆写——注意**不是**惰性的
   `SlatedbIter` 包装：它的 `next()` 调 `block_on`，在异步调用方所在
   runtime 内会 panic，惰性适配器的合法居所只有同步世界的 `SlatedbSync`）；
   异步 `commit_batch`（重放默认；SlatedbStore 用原生 `WriteBatch` +
   `await_durable` 覆写——slatedb 真实的一次写原子形态）；带值前缀对扫描
   以自由泛型函数落地（两个世界同一个形态）。异步 trait 本来就是
   `&self` 原生；ADR-0026 §2 早已点名它是"反例 trait"。

净接口面：put / get / del / scan_suffix / scan_range / scan_range_iter /
commit_batch——七个方法。除 `scan_range`（13 处）外每个方法都有 ≥13 个
外部调用点；`scan_range` 虽是 13 处但承重：远端路径的 eager 读就是
**单个 OP_SCAN 帧**，若从迭代器派生会把一帧拆成分页流。

## Why Not

- **保留 `KvBatch` 作为"面向未来的扩展性"。** 扩展性在结构上就是死的：
  `batch(&mut self) -> MemBatch` 把 carrier 类型钉死，原生 builder 无法
  满足这个 trait。填不上的插槽不是灵活性，是仪式（奥卡姆：实体必须以
  "解决了一个真实问题"自证）。
- **让 `commit_batch` 收 `impl KvBatch`。** 同样的陷阱上移一层——只有
  `MemBatch` 实现它，而引擎消费的是 `&[(key, Option<value>)]`，那**就是**
  `MemBatch.ops`。直接传列表，说的就是引擎真正需要的那一件事。
- **删掉 `scan_range`（理由："可由 `scan_range_iter` 派生"）。** 代码里
  可派生，协议上是错的：`RemoteStore::scan_range` 是**一次** OP_SCAN 往返；
  派生形态会为想要缓冲答案的调用方走分页（ADR-0021 chunk 协议）。两个都
  留——它们是两种成本，不是两种形态。

## 未来形态（暂缓，触发条件记录）

若出现被证实的需求：不经中间 `Vec` 直接流式写入**原生** builder（超大
批次），或批次级引擎选项（compression、ttl），正确形态是关联类型：

```rust
trait VirtualStorage {
    type Batch: KvBatch;
    fn batch(&self) -> Self::Batch;   // builder 出生即持有引擎句柄
    // 没有 commit_batch：提交点在 builder 上
}
trait KvBatch {
    fn put(&mut self, key: Vec<u8>, value: Vec<u8>);
    fn del(&mut self, key: &[u8]);
    fn commit(self) -> Result<(), String>;
}
```

模型层的组合靠 builder 泛型（`save_into<B: KvBatch>(&self, batch: &mut
B, ..)`）；要长期持有 builder 的结构体才写 `S::Batch`；常见形态（就地构建、
就地提交）根本不用写出它。

诚实的代价清单（2026-09-26，经 dyn 质询后修正）：
- dyn 兼容性：损失**接近零**。`&dyn VirtualStorage` 全树只有两处——
  `variant_column`（parquet 私有助手，泛型化是一行改动）和 derive 生成的
  **空钩子** `__okm_embed_deref`（函数体 `{}`，参数未用）。okm/aura 的多
  引擎运行时分发走的是**静态 enum**（`TestStore` 变体、
  `MqEngine::Fjall|Test`），erasure 这条路从来没被走过。
- 真实成本：每个 enum 转发 impl 要长出一个 enum builder（每方法一层
  match）；`NestStorage` 的读前 flush 模式（空 `commit_batch` 让同帧先写
  的 op 对后续读可见）变成 `store.batch().commit()`——对 redb 这是白开一
  个真事务，比今天空 op 列表的一次 no-op 提交更贵。
- 当下收益：未观测到（最大批次 ≈ 100 op，engine-matrix 测试）。触发条件
  出现前不做。

Update（2026-09-26，经 NoopBatch 质询后）：关联类型形态被排除为普适契约
——不是"暂不支持"，是对 redb 结构性不可能。`WriteTransaction<'db>` 借
`&self`，redb 4.2.0 没有 owned 事务类型；而关联 `type Batch` 必须是 owned
的（`fn batch(&self)` 按值返回，返回时借不了 self）。所以 redb 只能拿两样
东西填它：op 列表 carrier（等于 MemBatch 换名），或 NoopBatch 占位。占位
的退化路径二选一：逐条 put/del 重放（丢掉 redb 今天真实的单事务原子性——
倒退），或"占位"内部攒 Vec（= MemBatch 换名）。两条都不如现状。fjall 和
slatedb 确实能承载原生 builder（fjall `Batch` 是 owned、slatedb
`WriteBatch::new()` 本就 detached）——但普适契约的短板由最弱引擎封顶，且
这与 `KvBatch::commit(self)` 的死因同构：签名里不持有引擎的 builder 永远
够不到引擎的提交点。若流式需求真的触发，正确做法**不是**给
`VirtualStorage` 加关联类型，而是把各引擎的原生 builder API 在能成立的
地方上浮（okm 通用层只服务能服务的）——那是另一个 trait，不是这个 trait
上开洞。现行 `commit_batch(op_list)` 就是最小公分母契约：对 redb，
"提交时 begin_write + 灌入列表"本来就是它的原生形态。

## Mitigations

- 调用点审计可复现：上文计数来自会话中记录的 grep 模式，复审前重跑即可。
- 自由函数保留原名，模型层 diff 是机械替换
  （`store.scan_suffix_kv(&p)` → `scan_suffix_kv(&store, &p)`）。
- 异步侧工作（mudra R3）继承的是**变小后的**接口面：镜像 7 个方法，
  不是 9 个。
