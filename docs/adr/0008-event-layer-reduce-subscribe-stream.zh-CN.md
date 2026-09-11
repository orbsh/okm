# 0008 — 事件层：reduce、订阅通道、okm-stream

> **Languages:** [English](0008-event-layer-reduce-subscribe-stream.md) (primary) · [中文](0008-event-layer-reduce-subscribe-stream.zh-CN.md)

**Status:** Accepted（设计定案；实现待做——分阶段计划见 PLAN.md）

## 背景

跨行预聚合（ADR-0005 的 slot 空间，2026-09-10 落地）引入了 `#[kv_aggregate]` 辅助设施：
用户实现 `AggregateLogic`（Acc + fold/unfold），由写路径上的读-改-写 hook 驱动。审视这个
hook 的本质时浮现出一个泛化：fold/unfold 就是**对行事件流的内联消费**。同一事件源还能
支撑别的消费者——触发器（无状态副作用）、外部通知（观察者）、FRP 风格的流组合。本 ADR
记录把机制泛化的决定，以及守住边界的条款。

## 决定

### 1. aggregate → reduce（改名，语义不变）

`#[kv_aggregate]` / `AggregateLogic` / `AggCodec` / `aggregate_get` / `scan_aggregates`
更名为 `#[kv_reduce]` / `ReduceLogic` / `ReduceCodec` / `reduce_get` / `scan_reduces`。
改名让名字对齐角色：reduce 是对行事件流的有状态、可逆归约（put 时 fold，delete 时
unfold）。行为零变化；双层 trait 拆分（用户实现 Logic、derive 在其上实现 trait——
coherence 所迫）不变。

### 2. 单一事件源、两类消费者——这是承重边界

写路径在既有 hook 瓶颈点（`Table::put`/`delete` 里的 `Row::__okm_apply_*` 调用位）发出
**行事件**（表身份、key、op：put/delete、行 payload）。两类消费者挂在它上面，失败语义
刻意不同：

- **内联消费者**（`#[kv_reduce]`、触发器）：写路径内的同步调用，与写入同序，每事件恰好
  一次。reduce 的可逆性契约（`unfold(fold(a,x)) = a`）只在恰好一次的前提下成立——丢
  一个事件是静默的 acc/行失配（数据损坏，不是降级）。因此内联永远不是通道消费者。
- **通道消费者**（`#[kv_subscribe]`）：行类型上的每个注解点只声明"该行的事件进通道"——
  derive 在写路径发出的只是统一格式的发送（行身份、key、op、payload），注解点不挂
  handler：处理逻辑完全属于通道消费者，流 crate 的组合子是原始事件与那段逻辑之间的
  适配层。投递是
  **无保证的尽力而为**——有界/无界与丢弃/阻塞策略是订阅方的声明，不是核心的承诺。这
  是观察者模式的诚实形态：通知容忍丢失。

触发器的不对称（记录在案，不做统一）：reduce 可逆（unfold）；触发器是至多一次的内联
副作用，没有事务边界——"已发出的通知""已写进的另一张表"没有撤销。同一事件源上的两种
失败语义保持为两件事。

### 3. 通道在核心（derive 生成），不在扩展 crate

- `#[kv_subscribe]` 是 `RowEncode` 结构体的新属性；它只声明"该行的事件进通道"——每个
  注解的行类型得到一个统一格式的发送（注解点不挂 handler；处理归消费者，组合子是
  适配层）。
- 某行类型存在 ≥1 个 subscribe 声明时，derive 生成**全局通道声明与消费端访问器**
  （`okm_core::events::<R>()` 形态）。proc-macro 无法拥有真正的进程级全局——static 是生成
  模块里 per-row-type 的 `OnceLock`。跨行类型的流聚合是流 crate 的 merge 组合子的
  事，不是核心的事。
- 事件格式统一：(行类型身份，key 字节，op，payload 字节)。核心定形状；handler 按需解码。
- 异步边界：生产端同步（`try_send`，微秒级，写路径不 spawn）；消费端是
  `tokio::sync::mpsc`（默认无界——丢失容忍就是声明了的语义；有界+策略留作后续选项）。
  **核心保持同步。** Fjall 的阻塞 hook 是调用方的嵌入关切（`spawn_blocking` 属于嵌入
  侧，不属于 okm-core）；slatedb 的 async 藏在 `KvEngine` 之后；全核心 async 化什么都买不到，
  却让每个 API 付出代价。

### 4. okm-stream：通道之上的 FRP 组合子（新 crate）

新 workspace crate `okm-stream` 消费核心发出的 receiver，提供 Rx 风格组合子——
map/filter/merge/scan——以及推模式的多表 fan-in（多表流 merge 进一个 reduce）。它
**零存储职责**、不加原语；拉模式的多表 fan-in（scan + RMW，不经事件流）留在
`okm-query`。命名：crate 以它做的事（流处理）命名，不以字节来源命名；总线在核心。

### 5. okm → okm-core 改名

运行时 crate 更名 `okm-core`（workspace 成员、目录、crate 名、所有 `okm_core::` 引用）。
derive crate 保持 `okm-derive`。**引用全部更新，包括 ADR**（用户 2026-09-10 决定：
全部改）——ADR 仍是决策档案，但其代码引用予以修正，保证 grep 的真实性。顺序：改名
先落地，后续每个阶段都建在新名字上。

## 落选方案

- **把触发器/通知/reduce 统一成一套基于通道的机制。** 否：通道投递给不了 reduce 的
  恰好一次，硬凑（有界+重试）等于把集成边界排除在外的分布式协议语义请回来。内联 vs
  通道是结构性拆分，不是偏好。
- **Spacetimedb 式 reducer（reducer 即写路径，事务性存储过程）。** 精神相同（行事件
  驱动状态），结构不同：OKM reduce 是声明式、自动可逆的（更接近物化视图）；Spacetimedb
  reducer 是手写写路径逻辑加客户端 SQL 订阅。不采纳；对比时不要混同。
- **事件总线放 okm-stream（扩展 crate 拥有通道）。** 否：那样写路径就要依赖扩展 crate，
  层次倒置。核心发射；流 crate 组合。

## 边界

- 核心端到端保持同步；async 只存在于通道的消费端。
- 核心只承诺机制：通道存在、事件按写入序发出、形状统一。投递保证、过滤、变换、跨行
  类型扇出都是下游关切（`okm-stream` 或应用层）。
- 多写者/分布式事件投递继续出界（单写者纪律，与 reduce 的 RMW hook 同一边界）。
- 无事件重放/持久化：通道只活在当下；持久的变更数据捕获是引擎层能力（fjall watch），
  不进模型层。
