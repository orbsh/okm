# ADR-0017：图 Edge——第三种关系载体

日期：2026-09-19
状态：已接受（2026-09-20；两种形态均已实现——EdgeEncode derive + Graph<S, E> 固定本体、okm-dynamic Graph<S> 全动态）。2026-09-20 更新：§2 端点引用改选方案 b（自描述 [ns 2B][len varint][pkey]）——方案 c（注册表）废弃；§3 增补声明属性字段、节点侧 kind 过滤、slot 布局说明。
关联：ADR-0015（关系分类学）、ADR-0016（4 字节头、段号制 slot）、ADR-0002（命名空间字典）

## 背景

关系分类学（ADR-0015 §3）有两种载体：`Refs<D, K>` 承载一对多（字段位、单向、固定父类型的孩子），`Junction` 承载多对多（独立条目、双物化、双端编译期定类型）。两者都把端点**编译期绑定到已声明的文档类型**——derive 解析 `<D as Document>::Key` 与 `NS_PREFIX`。

图应用打破这个绑定。知识图谱或 LLM 写入的属性图具有：

- **异构端点**——一个图连接多种类型的节点（Person、Org、Document……）；为每种端点类型组合声明一种 Edge 是组合爆炸，且 PGQ 语义允许任意节点表参与。
- **运行期涌现的 kind**——边的 kind（"has"、"owns"）是数据，不是编译期词汇表。全动态形态下节点 kind 同理。
- **边的身份与属性**——平行边存在（同两节点间的两个 "has" 是两个事实），边携带动态属性。两种载体都不支持：Junction 的身份就是端点对，且两者都没有 per-edge 载荷。
- **独立存在**——边在查询范围内先于或后于端点文档存在；边集是一等集合，不是派生视图。

`Refs` 表达不了（孩子类型固定、字段位、单向、无属性）。`Junction` 也不行（端点类型固定、无边身份、无属性）。图 Edge 是**第三种关系载体**。

## 决策

### 1. Edge = 一等集合，每图一个 ns

一个图的边住在自己的 Edge collection——一个图的边集一个 ns，走正常 ns 字典分配（ADR-0002，"字典是代码"），与任何文档集合对称。无新命名空间机制：不是每种边 kind 一个 ns（动态形态下 kind 是运行期数据），不是全局边池（图在 ns 层隔离），不是寄生（边独立于端点文档存在）。

同一个 ns 承载边集的所有条目种类，按 ADR-0016 的段号 slot 分派。

### 2. 自描述端点引用

端点**不是编译期定类型**。引用是 `[ns 2B BE][pkey]`——任意 collection 的任意文档、任意 key 形状。ns 充当类型标记：解码引用按 ns 路由到归属 collection，节点类型不受限。

pkey 的边界**不在 key 里**。一个 collection 的 pkey 宽度是 schema 事实（`KeyEncode::KEY_LEN`），Edge 实现通过 **ns → KEY_LEN 注册表**解析。三个候选方案经过权衡：

- **a. 定宽约定**——要求同一图内所有节点 collection 的 pkey 宽度相同（如都是 u64 id）。key 零开销，但对建模是硬约束：一个 20 字节复合键的异构节点就破坏它，且跨图复用注册表会变别扭。拒绝——对真实图太死板。
- **b. 自描述段（TLV）**——`[ns 2B][pkey_len varint][pkey]`。完全通用，任意 pkey 形状通用，解码无需注册表。代价：每条引用 +1 字节以上（len 前缀），且每次解码多一个 varint 步骤。拒绝——Edge collection 持有数千条引用，per-reference 开销会在遍历面（0x5-0x8，存储中最高频的条目）上成倍放大。
- **c. 注册表查表（选中）**——pkey 宽度来自 schema（`KeyEncode::KEY_LEN`）；Edge 实现持有 **ns → KEY_LEN 注册表**（声明形态编译期生成，动态形态运行期表），按查找切出 pkey。key 零开销，把 Ref 纪律——"引用是 key 字节，ns 知识在声明处"——从单一绑定的端点类型推广到端点类型的注册表。注册表是新的跨集合 schema 事实：编译期形态生成，动态形态用户维护。

- **2026-09-20 更新——改选方案 b（自描述引用），方案 c 废弃。** 原草案对 b 的拒绝算错了账：共享 varint 编解码器把 pkey 宽度（4–16 字节）编进**单字节**，每条引用实付约 1 字节（遍历面 key 约 5%），不是"1 字节以上成倍放大"。与之相对，注册表的代价是全套：新的跨集合 schema 事实、`nodes(...)` 声明里手写宽度字面量与节点 key 的漂移风险、动态形态的运行期 `register()` 仪式，以及一个只为保卫注册表而存在的解码失败模式（未注册 ns）。引用变为 `[ns 2B][len varint][pkey]`——宽度是 wire 数据，遍历面依然精确前缀命中（同节点 = 同字节），两种形态的端点声明归零。Ref 纪律就此对图边**退役**：ns 知识不再住在声明点，而住在引用自身。

### 3. Slot 分配（ADR-0016 段，在 Edge ns 内）

```text
0x0  主表      [edge_id]                                  → value = 边本体
0x2/0x3  kind 字典  （name ↔ kind_id，复用既有机制）
0x4  kind 索引  [kind_id]                                  → edge_id 集合
0x5  出边       [src NodeRef][edge_id]
0x6  入边       [dst NodeRef][edge_id]
0x7  类型正向   [kind_id][src NodeRef][edge_id]
0x8  类型反向   [kind_id][dst NodeRef][edge_id]
```

**Slot 布局与普通文档的关系（2026-09-20 更新）**——明确说明，不追求严格一致：

- **节点 collection = 标准文档布局，不变。** 节点 kind 面不是新段：它是 0x1 段里一个普通访问方法（固定本体形态是一条 `#[ok_index]` 声明，动态形态是用户分配的 `AccessMethod` slot）。节点 kind 走 collection 自己的 0x2/0x3 字典——每个 collection 都有同一本字典。与任何文档 collection 布局零差异；"某个字段是 kind"是建模约定，不是布局事实。
- **Edge collection = 上表八类条目，用同一套段语言写成的另一种 collection 形态。** 0x0/0x1/0x2/0x3 保留文档侧的段语义（主表、声明索引、字典）；0x4–0x8 是 Edge collection 的普通 ns 内段（ADR-0016）——不引入新段类型。Edge 不是"带特殊段的文档"，而是由共享词汇组成的另一种 collection。

- **主表（0x0）**：边本体——`[src NodeRef][dst NodeRef][kind_id][attributes]`。属性是动态段（value 内的 slot-1 帧）：图属性天然动态。
- **声明属性字段（2026-09-20 更新）**：固定本体形态下，边声明可以携带 src/dst/kind 之外的静态字段。每个声明字段就是一条普通声明索引——segment 0x1（`0x1001+n`，声明序），每字段一面 `[字段值][edge_id]`。derive 直接生成访问方法；过滤走该面的索引扫描再解码回主表，而不是全量取边解码。字段值是边自身属性——不含 NodeRef，天然定宽——因此完全不触碰 ns → KEY_LEN 注册表。与 kind 字典互不相干：kind 是运行期增长的词表，声明字段是编译期声明、直接编码进索引 key。默认不建复合面（`kind+字段`、`字段+端点`）——用选择度最高的面过滤，edge_id 在内存收敛，与文档索引同一立场；复合面只在访问模式证明需要后再加。动态形态不受影响（空声明，属性全在动态段）。每个声明字段使 link/unlink 多写一条条目——写放大由声明者自选。
- **节点侧 kind 过滤（2026-09-20 更新）**：按节点 kind / 节点属性过滤是图查询的基本需求，按形态分别回答。固定本体形态下节点就是普通 document collection：节点"类型"= collection 本身（ns 即类型标记），属性过滤走 collection 自己的 `#[ok_index]` 声明索引——零新机制。全动态形态下节点 kind 是动态段数据，由两个既有机制承载过滤、不新增段：(1) **节点 kind 字典**——每个 collection 本来就拥有自己的 0x2/0x3 字典，节点 kind 走它分配；节点 collection 的字典与边 collection 的字典天然分开（字典是 per-collection 的）。(2) **节点 kind 面**——节点 collection ns 内的一个声明访问方法，`[kind_id][node pkey]`，由节点正常 put 路径维护。复合过滤（`(*)-[edge_kind]->(:node_kind)`）扫 0x7/0x8 面与节点 kind 面、内存求交 id 集合——两个便宜面、一次收敛（不为它建三元复合面）。
- **kind 索引（0x4）**：一种 kind 的全部边——`MATCH ()-[r:has]->()` 的入口。`kind_id` 来自 kind 字典（0x2/0x3）：写入时 name → id，保证每个 key 段定宽可排序。
- **遍历（0x5/0x6）**：一个节点的全部出边 / 入边。完整 NodeRef 领先使扫描前缀可命中；`edge_id` 收尾——平行边（同两节点间的两个事实）各得一条条目，尾段 id 解码回主表。
- **类型遍历（0x7/0x8）**：`(*)-[kind]->(*)` 面——kind 领先，因为它在此面是过滤维度；kind 不能作为此面的尾段。
- **写入协议**：`link` = 主表 + 0x4 + 0x5 + 0x6 + 0x7 + 0x8 一个 batch（六条条目）；`unlink` 对称删除六条。与 Junction 的双写义务同构，按面数放大。

### 4. 两种形态，一套机制

- **固定本体**：边 kind 与节点集合编译期声明。Edge derive 从 `#[ok_edge]` 形态的声明生成六面条目（声明属性字段每字段额外加一条 0x1 面，见 §3）；ns 注册表是编译期常量。
- **全动态**（知识图谱）：一个 `KgNode` collection 与一个 `KgEdge` collection，**空声明**——只有身份，载荷全在动态段（声明 `props: DynamicValue` 会把它放进 slot 0，那是静态形态——不是动态图想要的）。kind 与节点 kind 是动态段数据；端点注册表是运行期表；遍历面通过动态层的 `AccessMethod` 构建走。

两种形态共享同一 wire 布局与同一 slot 语义；差异只在声明住在哪里（derive 常量 vs 运行期 schema）。

### 5. 分类学中的位置（ADR-0015 §3 修订）

三种关系载体，按两根轴划分——端点定类型与边的独立存在性：

```text
                    端点类型固定                  端点类型开放
单向                Refs<D, K>                   —
双向                Junction                     图 Edge
```

图 Edge 相对 Junction 的结构差异恰好两点，都被开放端点的要求逼出：端点是自描述引用（ns + 注册表定界的 pkey）而非编译期定类型的 key；尾段是 `edge_id`（而非对端身份），因为平行边存在。其余一切——双写义务、条目面设计、ns 纪律——原样继承。

## 后果

- 新 derive（`EdgeEncode` 的变体或独立的 `GraphEdgeEncode`）+ 一个小的运行期注册表类型。六面写入协议住在 `Graph<S, E>` 装配点，与 `Junction<S, E>` 对称。
- ns → KEY_LEN 注册表是新的跨集合 schema 事实；编译期形态生成，动态形态用户维护。
- ADR-0016 的段不改动：0x4–0x8 是 Edge collection 的普通 ns 内段。
- 尚无消费者；设计在 `#[ok_relation]` 工作开始前落档，因为两者共用写路径 diff 机制。
