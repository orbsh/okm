# ADR-0031: 批量写路径承载语义——`BatchStore` 适配器

**状态**：已接受（2026-10-20）——实现随本裁决落地（`BatchStore` 在
`okm-core/src/engine/storage.rs`，`Collection::save_with_prev_into` 在
`okm-core/src/model/document.rs`；`okm-core/tests/save_into_test.rs` 覆盖）
**日期**：2026-10-20
**英文版**：[0031-batch-write-semantics.md](0031-batch-write-semantics.md)

## 背景

`Collection::save_into(&mut batch, …)` 是跨表原子路径（ADR-0003）：几张表写进同一个
外部持有的 `Batch`，一次 `commit_batch` 让它们一起活或一起死。它自己的契约写着
它是**编码**面而不是**语义**面——它绕过 `put` 的高层写路径钩子：reduce 的 fold/
unfold、嵌入文档的释放、subscribe 事件。于是两件事全部压到每个批量写者身上：

- **参与 reduce 的表根本没法写进 batch。** reduce 是一次对组条目的读-改-写
  （`apply_row` 收 `&mut S where S: VirtualStorage`），而 `Batch` 只是操作收集器，
  没有 `get`：RMW 读不到当前累加器——也读不到**同一批**里已经为同一组产生的值，于是
  一批里的两次写各自从旧值起算，重复计数。
- **覆盖写会把上一份文档的索引条目留成悬挂**（`put` 的既有契约：条目键编码了被索引
  的值，值一变就落到新键上）。批量写者只要覆盖，就必须自己删旧条目，否则永久攒垃圾。

所以今天调用方只有两条路：放弃 batch（丢掉原子性），或者把引擎已经有的账本自己重写
一遍。第二条正是本 ADR 要消掉的那个失败——调用方自己发号、自己建序列表、自己写恢复
扫描，等于给一本已经存在的账开出第二个真相源。

## 决策

**一个适配器，让 batch 成为语义路径可以使用的写入器。**

```rust
/// 读先看 batch 的待写项、再穿透到 store；写只进 batch。只支持点读——见下方的
/// scan 条。
pub struct BatchStore<'a, S: VirtualStorage> {
    store: &'a S,
    /// batch 放在 cell 后面，因为 `VirtualStorage` 的写操作取 `&self`
    /// （ADR-0026）；操作列表仍归调用方所有。
    batch: RefCell<&'a mut Batch>,
}
impl VirtualStorage for BatchStore<'_, S> { /* get / put / del，读穿透 */ }
```

1. **读穿透，按这个顺序。** `get` 先查 batch 中该键最新的待写项，再查 store；
   对某键 `del` 会遮蔽它的待写 put，于是「先删后写」与「先写后删」在同一批里都答对。这正是让同一
   组的 fold 看见本批自己产出的值的那一步。
2. **scan 一律拒绝，而且大声拒绝。** 把 batch 的待写项与 store 级迭代器合并不是点读，
   reduce 路径也不需要。scan 方法的返回类型是 `Vec`、没有错误通道，所以 `BatchStore`
   直接 panic 出契约，而不是给一份部分视图（所有扫描形态都汇到 `scan_range`）。
   批量期间要扫描的调用方建模的是另一种操作（先读、再判断、然后开一个批）。
3. **`Collection::save_with_prev_into(batch, key, doc, prev)`**——`save_into` 的语义
   兄弟：主表 + 面（同今天）、把 `prev` 不再产出的面删掉、把 `prev` 持有的嵌入子文档
   中新文档不再引用的那些释放掉（与 `put` 一致）、并通过 `BatchStore` 折/回折
   reduce。`prev` 由调用方给（常见情形：它刚读过旧行），为 `None` 时经适配器读。
4. **一次提交，一次账变。** 经适配器写出的 reduce 条目就是普通 batch 操作，所以一个
   不提交的 batch 不留任何累加器变化——账本的原子性从此与行一致。
5. **subscribe 事件留在 batch 路径之外**——明说缺席，而不是悄悄丢。批量写者的事件归
   它自己；在未提交的批里发事件等于宣布可能永远不会落地的写。

## 后果

- 同时需要多表原子写和 reduce 的调用方，现在两样都有：发号（`HighWater`）、计数
  （`Count`/`Sum`）、以及任何手写 `ReduceLogic`，都能声明在经 batch 写入的表上。
- 按这个用法存在的手搓替代品随之退役：私有序列表、它的恢复扫描、写者里那段「删旧面」
  的代码，全部收成一次调用。这就是目的——账与索引维护是引擎的，调用方只声明 schema。
- 单写者边界不变（ADR-0023「单写者」）：RMW 仍然在一个 store 句柄上的一个串行写者里
  完成，不引入 CAS。
- 提交之前 batch 对读者不可见，因为读不查它——并发读者下的吞吐不受影响。
- 欠：批量写者的 subscribe 事件（它自己的裁决：提交时发，还是留给调用方）；
  `save_into` 的弃用路径，等 `save_with_prev_into` 覆盖全部调用点之后。