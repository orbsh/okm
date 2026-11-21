# ADR-0077：图的面留在 core，因为这条边界只值 3 KB

**Status**：2026-11-21 裁决。除一处读取位置修正外无代码：plan 与变更日志原本从 `model::hyper` 取槽位带，而带定义在 `model::layout`（`hyper` 只是转发），现在直接问 `layout`。PLAN 无对应阶段——没有要实现的。

边是 okm 的三个原语之一，而绝大多数应用一条都不声明。这个不对称是真的：`model/` 里图的半边是 8,456 行（总共 21,235），从不写边的 store 一行也不碰。所以这篇 ADR 回答的不是"图代码大不大"，而是"不用它的应用是否为它付账，付多少"。

## 背景

这半边包括：typed graph（`model/graph.rs`，2,304）、超边/簇层（`model/hyper.rs`，3,074），以及两种 schema-free 形态（`model/dynamic_graph.rs` 1,094、`model/dynamic_cluster.rs` 1,984）。

这五个文件之外谁引用它：`okm-lake`（8 处——给快照与恢复实现的 `EdgeTable`/`EdgeRestore`），以及各 binding（Python 与 steel 各有整套图表面）。`okm-query`、`okm-ngram`、`okm-vector`、`okm-stream`、`okm-series`、`okm-log`、`okm-async`、`okm-wire` 都是零引用。

桌上有三个杠杆：把面抽成独立 crate、用 feature 门关掉、或者原地不动。

## 测量

全新 target 目录，各跑两遍，同一 commit；消费者只声明一个 document 类型，写一行读回来，不声明任何边。

| | 编译（release） | rlib | 链接后的应用（内存引擎） | 链接后的应用（fjall） |
|---|---|---|---|---|
| 面在 core 里 | 5.42 s | 7.82 MB | 722,152 B | 2,414,144 B |
| 面被关掉 | 4.89 s | 5.17 MB | 719,416 B | 2,411,168 B |
| 差值 | −0.53 s（−10 %） | −2.65 MB（−34 %） | −2,736 B（−0.38 %） | −2,976 B（−0.12 %） |

debug 同向：4.23 s → 3.89 s，11.43 MB → 9.50 MB。

## 裁决

**1. 面留在 core。不拆 crate，不加 feature。**

**2. rlib 那个数字是骗人的。** rlib 里放着所有编译产物，链接器只取被引用到的。那 2.65 MB 从不进入任何二进制——应用真正付出的是 3 KB，接上真引擎之后是千分之一。看上去很大的成本是"编译"的成本，不是"携带"的成本。

**3. crate 边界会向"声明了边的用户"收费。** derive 在每一份 `#[ok_nodes]`/`#[ok_edges]` 展开里写出 `::okm_core::NodeRef` 和 `::okm_core::EdgeKey`，把面搬出去就需要 serde 式的 `crate = "::okm_graph"` 路径，落在每个图声明上——这是对用这个特性的人逐声明收税，去替不用的人省 3 KB。同一次搬迁还会把槽位带（定义在 `model::layout`）与读它的 key 构造分开，而那正是 ADR-0076 花了一整轮合上的缝。

**4. feature 门买到同样的 3 KB，代价是 cfg。** 需要九个 `#[cfg]`（五个模块、三处 re-export、`CollectionSchema::of_graph`）、四个测试目标的 `required-features`、两处改从 `layout` 读带——而 cargo 的 feature 是并集，任何链接了湖或 binding 的二进制照样拿到这些代码。只有"直接依赖 `okm-core`"的消费者真正受益，而它省下的是 0.53 秒的一次性编译。

**5. binding 也不拆。** 它们是按语言分的，不是按能力分的。图/非图两套 binding 会把打包矩阵翻倍，换来同样那 3 KB，而图表面在每个 binding 里都是较小的那一半。

**6. 尺度论据是决定性的。** 链接了 fjall 的应用是 2.4 MB，图的面在里面占三千字节。在存储引擎旁边优化噪音不是优化。

## 被否

- **把面搬进一个叫 `okm-graph` 的 crate。** `okm-graph` 已经存在，而且是另一件事：149 行算法（标签传播、PageRank），迭代邻居扫描，工作区内没有调用方。把存储搬进一个以算法命名的 crate 会把两件事并成一件，而上面的 derive 路径成本在两种方案里都要付。
- **core 里的 `graph` feature。** 用十二个 cfg 点和一条并集警告换 3 KB 与 0.5 秒。
- **把每个 binding 拆成图/非图两版。** 打包矩阵翻倍，产物一分不省。

## 什么情况下重开

- 图长出**不是遍历**的算法面。算法已经有家（`okm-graph`）；面是存储，今天它们身上没有值得抽出去的计算。
- 出现完全不写 `::okm_core::…` 路径的消费者——手工生成声明的 no-derive 消费者会让 derive 路径从"税"变成"结构性约束"，那是这里唯一一条可按结构性而非按价格重估的前提。
- 出现某一类部署，使 core 的 0.5 秒编译或 3 KB 产物的差别相对引擎可测。今天没有；上面的数字是未来任何提案必须越过的下限。

## 后果

- core 保留那 8,456 行（只有它自己编译），不声明边的 store 看不出它们的存在。
- 槽位带只有一个家（`model::layout`），key 构造只有一个家（`model::graph`、`model::hyper`），这就是 ADR-0076 立下的；plan 与日志现在从前者读带，不再绕经后者。
- 测量被记下来了，这个问题不再靠直觉重开：下一个提案必须拿出比 3 KB 更大的数字。