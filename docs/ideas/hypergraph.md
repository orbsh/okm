# 想法：原生的超图（n 元关系）载体

**状态**：想法，记录于 2026-10-24。不是决定——没有 ADR，没有 PLAN 条目，这里没有任何东西被承诺。§1–§6 的提案是一份外部文档（中文，2026-10-24）的实质复述；§7 拿它的前提对照今天的代码检查，§8 列出真正的设计必须先定下的东西。

**2026-11-12 复审。** 这份提案写作时所针对的载体已经退役。`Junction`——以及 §1 引用的 `collection.rs` / `junction.rs` 路径——都不在了（ADR-0051，2026-11-04）；多对多关系现在是「键即那对身份」的 collection，反方向声明为键自身字节上的 lead face（ADR-0054）。§1 的两载体前提和 §8.7 的比较都由 §9 取代，§9 陈述现在能决定一切的那个比较：超边对**关系表**。

姊妹文档：`~/world/krystallizer/docs/ideas/hypergraph-population.md` —— 摄取侧（超边如何被提出并写入，且写路径里没有 LLM）。它移到了拥有写侧的那个服务：okm 不提供抽取、不提供学习器、不提供阈值，所以那一半是记忆层策略，只有它对这个载体的约束属于 okm。两者可以分开裁决，只有基数一项不行——这里成员数有界，就逼着管道在写之前先合并。

## 1. 主张

okm 只有一个专用的关系载体，而且它是二元的：

- `Graph<S, E>`（ADR-0017）——开放的 `NodeRef` 端点、自有身份（`edge_id`）、属性、平行边、运行时谓词（`okm-core/src/model/graph.rs`），面 `0x06`–`0x09`。

多对多根本不是载体：它是键为那对身份的 collection（ADR-0051），反方向声明为键自身字节上的 lead face（ADR-0054）。底下共享的 entry 布局和 `Graph` 借用的那套相同——ns、一字节固定面、索引段、主键尾。本节原先列出的载体 `Junction` 在 ADR-0051 中退役；模型保住了形状、丢掉了机器，因为 collection 已经具备 junction 需要手搭的全部能力。

一条图边绑定一个源、一个终点。提案的前提是：高阶关系（agent 记忆、多方审计、风险）经常一次绑定 3 个以上参与方——`{用户 A, 组织 B, 资产 C}` 共同参与 `[审计事件 X]`——把它建模成一个重构节点加 k 条二元边，要付 k 倍的往返，并且丢掉「这 k 条事实是一条关系」。

提案的载体：`#[derive(HyperGraphEncode)]` 加一个 `HyperGraph<S, E>` 句柄。超边被**物理重构**：一行 payload 加 N 个成员面。成员面让「包含成员 M 的每条边」成为最左前缀扫描；两张成员面求交回答「同时包含 M 和 N 的边」——提案把这条查询当作全部意义所在。

## 2. 提案的布局

```text
primary      [ns 2B][slot 0x00][edge_id BE]            → hyperedge payload
member face  [ns 2B][face slot][member NodeRef][edge_id BE] → empty / covering includes
```

提案陈述的规则：

1. 每个参与方是一个 `NodeRef`——与图边已经在用的那种自描述形态相同，主键宽度于是仍是线上数据。
2. 面键的尾是定宽大端 edge id，于是两次成员扫描按指针比较归并，无需反序列化。
3. 超图占据自己的段/槽范围，键不可能与文档 collection 或 junction 撞。

## 3. 提案的声明

```rust
#[derive(HyperGraphEncode, Clone, Default)]
#[ok_ns(1000)]
#[ok_entities(User, Org, Asset)]            // admitted member tables
#[ok_index(by_time { fields(audit_year) })] // faces on the edge's own payload
pub struct AuditIncident {
    pub audit_year: u16,
    pub risk_level: u8,
    pub predicate: String,
}
```

宏据说还会生成一个 head 类型（`edge_id` + `members: Vec<NodeRef>`）和一个 `HyperEdge` trait 实现。

## 4. 提案的 API

- `HyperGraph::link(edge_id, members, &payload)` —— 内部构造一个 `Batch`，于是 payload 行与全部 N 个成员面原子地出现。
- `iter_edge_ids_by_member(&NodeRef)` —— 单个成员面的前缀扫描。
- 每个成员的 `scan_edge_ids_by_member(&NodeRef)`，由 `okm_query::intersect(stream_a, stream_b)` 合成公共边集合。
- 跨 collection 批路径的 `_into` 孪生（`link_into`、`unlink_into`）；`unlink_into` 先读 payload 行，然后精确删除 payload 及它蕴含的每个面。

## 5. 开销主张

- 零解析：面键偏移由 derive 在编译期固定；解码是指针切片。
- 内存友好：2 字节 ns 而非字符串前缀；布局据称适合 LSM bloom filter。

## 6. 提案没有解决的东西

基数上界、成员重数、逐成员的角色/位置、edge_id 分配、带谓词限定的成员面，以及超图是一种新的声明种类还是图声明的一个基数。§8 把这些当作真正的设计工作。

## 7. 对照代码检查（2026-10-24）

每一行是提案断言的一个前提，以及仓库实际怎么说。

| 提案说 | 代码说 |
|---|---|
| 超图占用面 `0x0A` | `0x0A` 是 `okm-vector` 的 per-shard 图（`okm-vector/src/index.rs:33`）；图自己的面停在 `0x09`（`EDGE_PREDICATE_IN_SLOT`，`okm-core/src/model/graph.rs:81`）。在用的固定面：`0x00`–`0x0A`，`0x0B` 是退役的快照槽，`0x0F` 是表元数据（ADR-0043）——所以空带是 `0x0C`–`0x0E`，三个面 |
| 面槽 2 字节 | 固定面是 **1 字节**；只有计数族（`0x1nnn`/`0x2nnn`）是 2 字节——ADR-0030 第 45-46 与 85-97 行，由 `write_slot` 实现（`okm-core/src/model/index.rs:81-91`） |
| `NodeRef` = `[ns 2B][len u16][pkey]` | `[ns 2B][len **varint**][pkey]`，`len < 64` 时一字节（`okm-core/src/model/graph.rs:104-111`） |
| edge id 是 4B/8B | `u64`（8 字节大端）到处一致：`link`、`edge_ids`、面 |
| `#[ok_entities(User, Org, Asset)]` | 现有声明词汇是 `#[ok_nodes(EntityRow, MemberRow)]`，按类型取文档**表**；derive 读每张表自己的 `#[ok_ns]`（`okm-core/src/model/graph.rs:196-216`，ADR-0017 状态行，2026-10-21 更新） |
| `link(edge_id, members, payload)` | `Graph::link(&EdgeHead{src, dst, predicate}, &E, edge_id: u64)`（`okm-core/src/model/graph.rs:224-232, 373`）；N 成员的 head 是新形状，而 body 是固定的 `[src ref][dst ref][predicate_vocab_id][payload]`（`graph.rs:236`，`decode_body` 在 `graph.rs:259`）——payload 偏移可算，只因为有恰好两个 ref，所以开放的成员列表需要一个计数或长度前缀 |
| payload 索引是超图特性 | payload 是普通 obj（ADR-0012）：一份文档声明已经同时给出两个 codec 方向和它的 `#[ok_index]` 面，计数族 `0x1`（`graph.rs:39-46`） |
| `okm_query::intersect` | 作为惰性适配器发布：`a.intersect(b)` / `a.merge(b)`，作用于按键有序的流（`okm-query/src/lib.rs:66,88`），在键上是集合语义、每个不同键一个元组——扩张成对的 join 是调用方的取行循环，不是算子。今天的图面扫描仍物化成 `Vec`（`Graph::edge_ids`，`graph.rs:404`），所以新的 API 是流式的、**面**上的 k 路求交，尽管底下的 `VirtualStorage::scan_range` 已经可用 |
| `unlink_into` 是写前读 | 确认：它读主 body、解码，然后删掉确切的面以及 payload 声明的索引条目（`graph.rs:553-590`）。N 成员时这只在 body 带成员列表时才成立 |
| link/unlink 是一次原子提交 | 对批路径确认：文档与边共用一次 `commit_batch`（ADR-0031，`link_into` 的文档在 `graph.rs:382-395`）。有一处不对称沿用下来：词汇分配自己提交，所以外层提交失败可能留下孤儿的谓词/词汇条目——无害，且已记录 |

## 8. 开放问题（设计工作，不是记录）

1. **第三种声明，还是图的一个基数？** `GraphEncode` 的面 `0x06–0x0A` 是 src/dst 形状的：out、in、predicate+out、predicate+in。集合形状的图把 out/in 塌成一个成员面加一个 predicate+member 面，方向区分从边层消失。这是第二个载体还是 ADR-0017 的一次加宽，决定了下游的一切（声明、derive、unlink）。
2. **基数上界。** 有界 N（比如 ≤ 16）让 body 保住可算的偏移，也守住了零解析那条主张；无界 N 需要在 body 里放计数/varint 前缀，payload 偏移于是变成一个游标。
3. **重数。** 成员面键无法区分同一个成员的两次出现，于是重复的参与方塌成一条面 entry。若「同一资产两次、角色不同」必须可表示，成员身份就需要把位置或角色放进键里。
4. **逐成员角色。** 超图文献里常见的下一个要求（`approver`、`receiver`）。面键里放角色字节或角色 id 会加宽每个面，并把扫描顺序钉死成 role-first 或 member-first；留在 payload 里则键窄，但角色查询变成一次完整取行。
5. **面编号。** `0x0C–0x0F` 是空的；把 `0x07` 复用成「成员」会重新解释现存的图键，所以超图必须住在自己的 ns 里（ADR-0017 §1 已经做到一图一 ns），永不与一组二元边共用一个。
6. **成员校验。** `NodeSet` / `#[ok_nodes]` 是现成的声明集机制（一个声明，不占槽）。复用它，而不是发明 `ok_entities`，否则词汇会为同一个念头长出两个名字。
7. **现有模式是否已经回答了这个用例？** 关系表回答了（ADR-0051 + ADR-0054），而且它比本节原先比较的二元边建模是更强的基线：N 个成员是 N 行，`put` 在同一个批里写行和每个面，关系上的属性是普通可索引列，反方向是从键上切出的 lead face（于是不可能与键漂移），行还会经过变更日志进入 lake。对着这条基线，超边是拿 N 行换 1 行 + N 个面。§9 陈述它赢在哪、输在哪。

## 9. 超边对关系表（2026-11-12）

基线不是 junction，也不是「一行加二元边」。它是这样一个 collection：键是那对身份，反方向用 `#[ok_lead]`，关系知道的东西都是普通列：

```text
#[derive(KeyEncode)]  struct MembershipKey { channel_id: u64, member_id: Uuid7, joined_ms: u64 }
#[derive(DocumentEncode)] #[ok_ns(110)] #[ok_lead(by_member { fields(member_id) })]
struct Membership { nickname: String, since_ms: u64, role: u8 }
```

| 需求 | 关系表（今天） | 超边（提案） |
|---|---|---|
| 哪些关系牵涉 M | lead face `by_member` → 整键解码，取行 | 成员面 → edge id，再取 payload 行 |
| 一条关系有哪些成员 | 对成对键做前缀扫描 | 一次 body 读（N 个 ref 内联） |
| 同时含 A 和 B 的关系 | 两条 lead face 流求交（`OrderedSets`） | 两条成员面流求交 |
| 关系的属性 | 普通列 | payload |
| 成员**在关系内**的属性 | 普通列（`role`、`since_ms`） | **无法表达**，除非用角色/位置加宽面键，或者把逐成员数据存进 body 再取 |
| 成员有意义的顺序（join 顺序、序列） | 免费：多一个键列，换一条 lead face 重读 | **无法表达**：成员面把顺序塌掉了 |
| 「这 N 个成员是一条关系」 | 只是约定——缺一行也看不出来 | 结构性：body 加面一次写入 |
| 删除整条关系 | 扫前缀，再逐行 `purge` | 读 body，删掉每个蕴含的面 |
| 导出 / 变更日志 | 行就是行 | payload 行是；面是派生态 |

它赢的是那个形状的最后两行——每条关系一个对象，于是成员不可能被撕开，关系自身的属性只存在一次而不是 N 次——再加上索引条目少一半，因为方向没了，`out`/`in` 塌成一个成员面。

它输的是那上面一行，而对 agent 记忆来说那一行通常正是重点：关系很少是对称的（成员的角色、权重、生效时间、置信度都属于**那个成员在那条关系里**），而集合形状的载体没有地方放它，除非加宽面键或者付一次取行。它还输掉了「键即地址」这条便利——任何不可变的排序维度都能搭在成对键表的键里，再通过一个面回来，而超边的成员面没有地方放排序。

所以裁决点不在 §5 假设的基数上，而在**属性住在哪**：只有关系级属性时，超边确实更便宜、它的不变量也是真的；逐成员属性也要时，关系表已经能建模，超边就得在面键里把这能力重建一遍。真要造原型，值得造的是图的集合形状（复用只带方向的 `0x07`/`0x09` 及其带谓词的孪生，ADR-0017 §1 保持一图一 ns），不是第三个载体——而且要等工作负载显示「无逐成员属性的关系是常态」之后。

## 10. 讨论之后：两种别的形态（2026-11-12）

§9 把比较对象收成关系表，但讨论里还有两种形态，它们不是新的载体，而是把"多路关系"安置在别的位置上。记在这里，是因为它们改变的是判据本身。

**形态一：集合作为一种顶点。** 不新增 `HyperGraph`，而是给 `Graph` 加一种顶点——声明了成员类型的集合节点，成员边带谓词（`approver`、`receiver`、`evidence`）。这样它同时拿到超图的收益和超边丢掉的东西：关系是一个有 id、有属性的个体；成员各带角色与权重（属性在边上）；同一成员多种角色靠**平行边**（ADR-0017 本来就有平行边，超边的集合容器做不到）；成员次序靠边的键。相对关系表它多出来的是"关系可以在没有成员时也存在"和"属性只存一份"，付出的只是那个集合节点需要一个 id——而引用一条关系本来就需要 id。

一处纠正：集合节点并不限于一种成员边。边带谓词就是异构关系容器，"异构"从来不是超边的独有能力。

**形态二：子图边界。** 把一组节点和边作为一个整体，有 id、有生命周期、可嵌套、可整体删除或换版本。它增加的表达力落在**包含、嵌套、作用域**上，而不在"关系能绑几个端点"上——RDF 的 named graph 是同一件事的先例，图作为一等值。代价是边界必须被强制，否则只是标签；而强制只有两条实现路径，都落在现有原语上：每个节点与边带 `scope_id`（靠纪律，越界不拦），或容器的 body 列成员引用（强约束，等于超边的成员表但成员异构）。真正的坑是身份：一个节点在边界内、同时作为实体在别处存在时，要么共享（"在边界内"就只是标签），要么拷贝（身份重复、改一处不同步）——这正是 `MODELING.md` 里"存一起高效但耦合、分开归一化但要一跳"的同一个张力搬到图这一层。

**判据。** 三个被混在一起的概念要分开：

- **聚合**——"这些东西属于一个东西" → 集合节点（带谓词边）就够；
- **包含**——"这些东西只在这个东西内有效，删容器即删内容" → 子图边界；
- **嵌套**——"子图里还有子图" → 子图边界加层级。

超图要解决的是第一种，子图边界解决的是第二种，二者不该互相替代。所以结论不变：**不要新建 `HyperGraph`**；集合节点作为 ADR-0017 的一种顶点形态（复用 `edge_id`、平行边、`link_into`/`unlink_into`、`NodeRef`），子图边界留给真正需要所有权语义的场合（案卷、事务、变更集、文档树），且实现为 `scope_id` 纪律加级联 helper，而不是把节点嵌套起来。
