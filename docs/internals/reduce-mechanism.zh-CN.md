# Reduce 机制：账本、覆盖写与可逆性

本文是 `#[kv_reduce]` 跨行预聚合的机制与实现细节文档：fold/unfold 钩子怎么驱动、覆盖写为什么要 unfold 旧值、以及这条不变量对写路径的约束。声明与建模纪律见[建模指南「跨行预聚合」](../MODELING.zh-CN.md)；索引侧的写路径同步见[索引机制](index-mechanism.zh-CN.md)。

## 定位

reduce 的 entry 结构与索引条目同款（`[ns][slot][group 段]`），但语义相反：索引条目是**随行生灭的派生视图**（一行 N 条 entry，行删条目删）；reduce entry 是**共享可变账本**——一个 group 的所有行折叠进同一个累计值，行删了，账上的贡献必须被撤回。这个相反的语义决定了它独有的约束：**账本对主表的行集合必须恒等**。

## 不变量

```text
acc(group) = Σ fold(row)   对主表中属于该 group 的每一行
```

acc 恒等于「主表当前所有行的 fold 之和」。破坏它的不是并发（单写者引擎下 get→f→put 不可交错），而是**覆盖写**。

## 为什么覆盖写会翻倍：账本无记忆

reduce entry 里没有成员清单——KV 里不存在「哪些行给这个 group 贡献过」的记录，账本只有一个累计值。因此钩子对「这行写了几次」无知，覆盖写时序（修复前）：

```text
put(k1, hits=10)   → fold(10)：acc = { count: 1, sum: 10 }
put(k1, hits=15)   → fold(15)：acc = { count: 2, sum: 25 }   ← 账实分离
```

第二次 put 是同一个 key——主表逻辑上只有一行 hits=15，但 acc 数学上对应两行（10 和 15），其中 hits=10 那行已经不存在。多记的贡献是永久账：后续 delete 的 unfold 减的是当时存储行的值，之前多记的部分永远还不掉，`unfold(fold(a,x)) = a` 的可逆性契约在这个序列上被打破。

根因不是 fold 写错，而是 **put 的追加语义与账本的共享可变语义冲突**：索引条目随覆盖写自然转移到新 key（entry key 编码了索引字段，旧条目悬挂另有 delete 兜底——见[索引机制「写路径的同步」](index-mechanism.zh-CN.md)），而账本在同一个 group key 下原地累加，无人替它平账。

## 修复：覆盖写先 unfold 再 fold

`Table::put` 现在读出主表旧行（slot-0 点读一次），存在则先从每个声明的 group unfold：

```text
put(k1, hits=15)   → get k1 → 旧行 hits=10 在手
                    → unfold(10)：acc = { count: 0, sum: 0 }
                    → fold(15)：  acc = { count: 1, sum: 15 }   ← 不变量恢复
```

要点：

- **插入路径零额外成本**：slot-0 miss = 没有可撤的账，直接 fold。只有覆盖写付这一次点读。
- **点读即终态前提**：这次读出的旧行正是「每次写都拿旧行」的来源（Phase 6 upsert_with 的基准态）——字段级 diff、消费端变化检测的免费副产品都立在这笔读取上，见 PLAN 事件粒度决策。
- **upsert_with 不另做账**：命令式 RMW 就是 `get → f(old) → put`，unfold 由 put 内部完成，入口不重复平账（否则撤两次）。

## 写路径的完整时序

```text
put(key, row)
  ├─ 读主表旧行（覆盖检测 + unfold 源）
  ├─ 旧行存在 → unfold(旧) 每个声明 group
  ├─ 写主表条目（slot 0）
  ├─ 写索引条目（每个 access method）
  ├─ fold(新) 每个声明 group
  └─ emit 事件（epoch +1，best-effort channel）
```

delete 是同一条路径的 unfold 分支（`add=false`，单调用点，`delete_by_pkey` 内部 get 后到达同一处）。
