# 0009 — FRP 对照：Spacetimedb 订阅 vs okm-stream

> **Languages:** [English](0009-frp-comparison-subscriptions-stream.md) (primary) · [中文](0009-frp-comparison-subscriptions-stream.zh-CN.md)

**Status:** Accepted（分析记录——不隐含代码改动；为 ADR-0008 的边界条款补上 Spacetimedb 这个参照系）

## 背景

ADR-0008 定下了事件层的形状：内联消费者（reduce，恰好一次，写路径同步）与通道消费者
（`#[kv_subscribe]`，尽力而为，组合子在 `okm-stream`）。Spacetimedb 的客户端订阅是
目前最接近的广为人知的对照系统——它在结构上就是一张长在数据库里的 FRP 图。本 ADR 记录
这场对照，防止后续设计讨论把 Spacetimedb 的 reducer/订阅语义悄悄搬进 OKM 的不同形态。

## 决定

### 1. 同一骨架：推模式的数据流图

两者是同一个物种——源 → 依赖 → 传播 → 观察者：

```
FRP:        source signal ──map/filter──▶ derived signal ──▶ observer
Spacetime:  表 delta ──查询计划求值(IVM)──▶ 订阅的行集变化 ──▶ on_insert/on_delete
okm:        行事件 ──组合子(okm-stream)──▶ 折叠出的状态 ──▶ 订阅方逻辑
```

客户端缓存就是一个 signal（持有最新值的可观察单元）；回调是 observer；SQL 查询是
组合子的声明式写法（`where` ≈ `filter`，`join` ≈ `combineLatest`）——只是求值引擎在
服务端，组合关系由查询计划表达而非代码链。

### 2. Glitch 处理：事务边界就是 FRP 的时钟

Glitch 指下游消费者观察到不一致的中间态——两个上游都变了，合并节点只被第一个输入
触发，用半份数据算出脏值，随后再重算。经典 FRP 用拓扑排序加原子（成批）传播消灭
glitch。

Spacetimedb 文档里的机制逐字对应，只是换成了数据库的名字：

- "每个事务恰好产生零或一条更新消息……原子的"——事务级批处理 = 按 tick 成批传播；
  订阅者永远看不到半个事务。
- "回调……被推迟到事务的缓存更新全部应用完之后"——observer 只读到传播完成后的
  一致状态，即 glitch-free 保证。
- 订阅初始化的快照取在"两个事务之间"——一致读。

结构优势：串行事务日志是现成的逻辑时钟——提交即 tick。RxJS 缺这个，把一部分
glitch 规避留给了开发者。

### 3. 组合子的位置：承重差异

组合在哪里求值，组合的表达力就被什么封顶：

- **Spacetimedb**——服务端查询引擎，增量求值（IVM："对查询求导"）。体验最好：
  网络上只传 delta，客户端缓存零计算。硬上限：查询必须可分析——join 订阅要求
  join 两列都有索引（删掉一个，订阅在客户端运行时报错）；聚合不做增量维护
  （聚合归 views，而 views 是黑盒代码，靠读集失效重算，不走 IVM）。
- **okm-stream**——消费端，对原始行事件的任意 Rust 组合子。无需分析，表达力无
  上限；代价是线上流的是原始事件、算力在订阅方。

交换是零和的：**求值权放在哪，哪一端的能力就给组合语言封顶。** Spacetimedb 敢放
服务端，因为查询引擎是它的原生器官；okm-stream 把组合放到消费端，因为 OKM 是库、
没有查询引擎可依。

### 4. 时间语义：只有离散，以及 signal vs stream

Spacetimedb 订阅是离散事件 FRP：世界以事务为单位更新，"两个事务之间"没有值可取。
连续 Behavior（经典 FRP 的另一半）在分布式环境里不存在——没有全局的"现在"可读。

还有一根轴：客户端缓存是 **signal 语义**（可读当前值）；OKM 的 mpsc 通道是
**stream**（可读下一事件，没有当前值）。把流提升回 signal 需要折叠——这正是内联
reduce 的角色：Rx 里的 `scan()`，放在核心是因为恰好一次与可逆性必须在写路径强制。
Reduce 就是"把事件流变回 signal，物化进 KV"。

## 边界

- OKM 不订阅查询。"在写端组合、下发结果"意味着在 OKM 内长出查询引擎 + 血缘分析 +
  IVM——那是另一层系统，与"流系统在旁边"是同一条边界（现在的 FRP 版证明：表达力/
  分析能力的封顶是结构性的，不是缺功能）。
- 可检验推论：okm-stream 用户普遍要求写端组合之日，就是库边界被突破的预警信号。
