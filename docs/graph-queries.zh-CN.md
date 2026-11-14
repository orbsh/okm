# 图查询配方：这些面如何组合

图边层（ADR-0017；身份见 ADR-0067）的查询配方：这些面如何组合成 Cypher 熟悉的形状，`NodeRef` 与句柄各自是什么，过滤发生在哪一层，以及一份真实设计的完整例子（按社区召回）。wire 布局见 [key 布局](internals/key-layout.zh-CN.md)与[建模指南](MODELING.zh-CN.md)，机制记录见 [ADR-0017](adr/0017-graph-edge.zh-CN.md)、[ADR-0067](adr/0067-an-edge-has-no-identity-of-its-own.zh-CN.md)。

## 核心立场

一个面 = 一个访问方法；图查询 = 先扫最 selective 的面、在内存收敛身份——没有组合面，没有查询引擎。边没有号：身份就是元组 `(src, predicate, dst)`，所以每个原语答的是 `EdgeKey`，而一次 walk 答的是句柄。载荷只对幸存元组逐条取。声明即执行，与 OKM 全库一致。

## NodeRef：使用方说的端点地址

`NodeRef` 是 API 说的端点地址——`[ns 2B][pkey]`，不带长度：

```text
┌─────────┬──────────────────┐
│ ns  2B  │  pkey（裸）       │
└─────────┴──────────────────┘
   类型          身份
  标记
```

- **`ns` 是类型标记。** 一张图连接多个 collection 的节点；节点属于哪个 collection 不是声明出来的，是 ref 里的两个字节。ns 字节与 `Document::NS_PREFIX` 同形（`[ns 2B BE]` 头）——节点引用由源文档自己的头加 key 编码拼装：`NodeRef::new(&<Org as Document>::NS_PREFIX, &key.encode())`。
- **宽度来自图的声明。** `#[ok_nodes(..)]` 是一组闭合的成员表，`KeyEncode` 定死每张表的宽度，所以一个 ref 只靠宽度就能解析：不同节点 collection 可声明不同 pkey 宽度（4 字节组织 id、16 字节 agent id）且共存。这就是端点开放的机制——任何文档 collection 零仪式参与，前提是它的键定宽。
- **同节点同字节。** 编码是规范的，所以元组精确匹配，混合宽度的节点不会互撞。

使用方说的不等于一行里存的：在图的键空间里，端点是八字节句柄（见下一节），`(ns, pkey)` 那一对活在登记册里。`NodeRef` 是边界类型——写入时进、读取时出。

`NodeRef` 与 `EdgeBody` 配对——一条边的 body 读取：

```rust
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub predicate_id: u16,     // 谓词名经 g.predicate_vocab_name(predicate_id) 还原
    pub payload: Vec<u8>, // 边自己的载荷——一份 obj 载荷，
                          // [version][hot_len][热段][冷 TLV]（ADR-0017 §3）；
                          // 全动态形态只留这个头，开放属性写进固定面 0x01
}
```

## 句柄：一次 walk 实际交出什么

一条遍历条目就陈述了整条边。`walk` / `out` / `inn` 产出的 `EdgeStep`，每个字段都来自条目的键——做一次跳转不读任何行，也不读登记册：

```rust
EdgeStep {
    far_handle: Handle,            // 这一跳远端的句柄
    head: HopHead {
        src_handle: Handle,        // 边的自身方向
        dst_handle: Handle,
        predicate: u16,
    },
}
```

所以一次 walk 交给你的是句柄，你解析你真正要问的那些：

- **一个句柄：** `g.node(handle)` → 登记册一次点查。`Handle::NONE` 永远解析不出句柄，登记册里没有的句柄是错误，不是猜出来的引用。
- **一组：** `g.nodes(&sorted)` → **一次**区间扫描，从首跨到尾（稀疏集合为跨度付费，不为整张面付费）。它收**有序**切片：句柄按发出顺序单调，所以按发出顺序收集的集合本来就有序，但 `EdgeKey` 的顺序是另一种顺序——显式排序，别假设。
- **完整元组：** `g.edge_key(&step)`，当你要 `(src, predicate, dst)` 而不只是这一跳给出的那半个身份。

由此有三条规矩，它们是契约而不是建议：

- **句柄是局部的。** 它是一个 ns 内图自己的编号：不出现在导出的 schema、不出现在载荷、不写进另一个 store 要读的文件。离开发出它的那张图，它什么都不是。
- **往返靠元组断言。** 再写一次同一个 `(src, predicate, dst)` 就是同一行；"同一条边"是关于元组的问题，从来不是关于句柄的问题。句柄是实现细节，你可以在一次查询期间持有它。
- **恢复之后编号可能不同。** 重建只重写反向面、不重发任何句柄，但把数据恢复到新 store、重建索引、或将来的 compaction 都可能给节点另一套编号。存下来的句柄不是地址；存元组，让图重新解析它。

成本：度数 *d* 的一次 walk 是一次扫描、零次登记册读。解析其中 *n* 个句柄是一次区间扫描（`nodes`）或 *n* 次点查（`node`）。贵的形状是在每跳解析，而答案根本不需要引用。

## 图的面，作为查询形状

前置：`Org(ns=9)` 与 `User(ns=10)` 上的 `employs` 图，写入两条边（组织 9 -> 用户 1，2020 年；组织 9 -> 用户 2，2021 年）。

```text
Cypher 形状                          okm 落点（面）
──────────────────────────────────────────────────────────
(:org {id:9})-[]->()                out_edges          (0x06)
()<-[]-(:user {id:1})               in_edges           (0x07)
(:org {id:9})-[]->(:user {id:1})    out_edges_to       (0x06，再过滤)
()-[r:employs]->()                  edges_of_predicate (0x08 裸前缀)
(:org {id:9})-[r:employs]->()       typed_out          (0x08)
()-[r:employs]->(:user {id:1})      typed_in           (0x09)
(:org {id:9})-[r:employs]->(:user {id:1})
                                    edge_at            (0x08，一个精确键：
                                                        断言同一性)
()-[r:employs {since: 2020}]->()    by_attr_face       (声明索引族 0x4nnn)
```

```rust
// 无类型邻接——触碰节点的全部边，不限谓词。
g.out_edges(&org9);   // -> [(org9, employs, user1), (org9, employs, user2)]
g.in_edges(&user1);   // -> [(org9, employs, user1)]

// 谓词过滤遍历——谓词领衔扫描前缀（0x08/0x09）。
g.typed_out("employs", &org9);       // -> 同样两条边，走限定的面
g.typed_in("employs", &user1);

// 成对查找。谓词夹在两端之间，所以 (src, dst) 不是连续前缀：这一条过滤源节点的
// 那段 run，代价就是它的出度。带谓词的那条是断言同一性——一个精确键。
g.out_edges_to(&org9, &user1);             // -> [(org9, employs, user1)]
g.edge_at("employs", &org9, &user1);       // -> Some((org9, employs, user1))

// 全图谓词扫描——`MATCH ()-[r:employs]->()` 的入口。
// 没有谓词面：这就是 0x08 的裸 `[predicate]` 前缀。
g.edges_of_predicate("employs");          // -> 两条

// 属性面——边自己声明的 `#[ok_index]`。对**整套声明字段**做等值匹配：探针必须
// 按声明序带上每一个字段。探针短了，剩余字段就挡在元组前面，**什么都匹配不到**，
// 而且是静默的——它不是前缀扫描。
let slot = <__OkmIndex_Employment_by_since as KvIndex>::SLOT;
g.by_attr_face(slot, &2020u16.to_be_bytes());  // -> [(org9, employs, user1)]
g.by_attr_face(slot, &1999u16.to_be_bytes());  // -> []（无面无错）

// body 读取——端点 + 谓词名 + 载荷，只对幸存元组。
let body = g.get_edge(&key)?.unwrap();
let predicate = g.predicate_vocab_name(body.predicate_id).unwrap(); // "employs"
```

所有原语答的都是元组；扫不到返回空 Vec——未知谓词与不存在的属性值都是空结果，不是错误。

## 组合：内存交集，无组合面

这些面刻意正交。单个面覆盖不了的查询（谓词 AND 端点类型 AND 属性）是若干面扫描的合取，在内存按元组交集（`EdgeKey` derive `Hash` + `Ord` 正是为此）：

```rust
// 组织 9 的雇佣边、2020 年入职、雇员是 User（ns 10）——三个面，最小结果集优先：
let since: std::collections::HashSet<EdgeKey> =
    g.by_attr_face(slot, &2020u16.to_be_bytes())?.into_iter().collect();
let candidates: Vec<EdgeKey> = g
    .typed_out("employs", &org9)?
    .into_iter()
    .filter(|key| since.contains(key))
    .filter(|key| key.dst.ns_id() == 10)
    .collect();
```

成本纪律与 OKM 全库一致：先扫最 selective 的面（等值属性匹配 > 谓词扫描 > 无类型邻接），让字节区间承担物理 where——内存交集只对幸存者付费。某个合取变热时，把它提升为边 collection 上的访问方法（对边自己的声明属性字段建 `#[ok_index]`），而不是在查询期组合——声明即注册，查询路径坍缩为一次前缀扫。

## 多跳：一串 step，一个节点访问集

多跳本身是广度优先 walk——frontier + visited 集——visited 集按 `NodeRef` 记（它 derive `Hash` + `Ord` 正是为此）。重访节点是 KV 侧花掉无界扫描预算的方式：防环不是可选结构，就是成本模型。每跳 = 每方向每边类型一次前缀扫；成本线性于触碰的边数，与图的大小无关。

```rust
// 雇佣关系的朋友圈形状：组织 -> 用户 -> 组织，两跳。
let mut frontier = vec![org9];
let mut visited: std::collections::HashSet<NodeRef> = frontier.iter().cloned().collect();
for _ in 0..2 {
    let mut next = Vec::new();
    for n in &frontier {
        for m in g.out_nodes(n) {          // 每次调用一次登记册扫描
            if visited.insert(m.clone()) {
                next.push(m);
            }
        }
    }
    if next.is_empty() { break; }
    frontier = next;
}
// `visited` = 两跳内全部节点，已去重。
```

`out_nodes` 是把每个 step 都解析掉的便捷形式。原始形态让 walk 留在句柄空间：`for step in g.walk(n) { … step.far_handle … }` 一个都不解析，所以答案只是计数、谓词判断或成员判断的 walk 根本不碰登记册；而要引用的 walk 按访问节点解析一次（`g.node`），或按调用把整个答案一次解析（`g.nodes`）。从句柄起一个流要花一次 `g.node(handle)` 点查，因为 `walk` 收的是地址，不是号。

walk 成为热路径时，把边 collection 的热谓词提升为声明的 `#[ok_index]`（见组合一节）——walk 的每跳扫描已经是物理 where，剩下的内存工作只有 visited 集。

## 按社区召回：采样节点，给它们的社区排序

这是一份真实设计，写下来是因为它的形状不显然：图里存着社区发现的结果，查询要回答"这一页结果落在哪些社区里"。整件事靠两个图动词——节点形态的 `clusters_of`，以及 `members`——其余都是策略。

**写入侧：一次运行的结果是一组簇。** 每个社区是一个簇，成员就是那次运行归到一组的边，kind 为 `Kind::COMMUNITY`。簇的载荷装的是**这次运行**的元数据——算法名、参数、产出的时间戳——因为簇行的载荷本来就是 `Dict`，模型里没有别的东西要为它改动：

```rust
let mut fragment = Fragment::<Employment>::new(
    community.edges.iter().map(|key| Decl::edge(EdgeHead {
        src: key.src.clone(),
        dst: key.dst.clone(),
        predicate: predicate_name.clone(),     // 这次运行只归一组关系
    })),
);
let mut meta = Dict::new();
meta.insert("algorithm".into(), Variant::Str("lpa".into()));
meta.insert("resolution".into(), Variant::F64(0.6));
meta.insert("produced".into(), Variant::U64(run_started));
fragment.cluster(Cluster::new(Kind::COMMUNITY, meta));
let report = graph.incorporate(fragment)?;   // report.hyper_ids = 这次运行的簇号
```

因为重算是写一个**新的**簇号、而不是覆盖旧的，多次运行可以共存——换个算法、换个参数、上周的答案——并且可比：`Hyperedges::clusters(Kind::COMMUNITY)` 枚举它们，`lookup` 取回载荷，按 `payload["algorithm"]` 过滤就选中某次运行。比较本身是按成员边做连接，而那正是图本来就在说的元组身份。

**查询侧：召回 100 个节点，然后数。** 应用从向量面召回一页——100 个 `NodeRef`，按相关度排序。它们的社区来自各自的关联边，每条边一次 `0x0D` run，所以代价是这一页涉及的边数，与图的规模无关：

```rust
let hits: Vec<NodeRef> = vector_recall(&query, 100);

let mut tally: HashMap<ClusterRef, usize> = HashMap::new();
let mut seen: HashSet<ClusterRef> = HashSet::new();
for node in &hits {
    // 流只折叠**相邻**的重复；一个节点若有两条边同属一个社区，它会报两次，
    // 所以每个节点只数一次。
    seen.clear();
    seen.extend(graph.clusters_of(node));
    for cluster in &seen {
        *tally.entry(*cluster).or_default() += 1;
    }
}

// 采样的节点做决定，簇是答案。
let mut ranked: Vec<(usize, ClusterRef)> = tally.into_iter().map(|(n, c)| (n, c)).collect();
ranked.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
let top: Vec<ClusterRef> = ranked.iter().take(3).map(|(_, c)| *c).collect();
```

在这个故事里，100 个节点中 30 个落在社区 A、20 个落在 B、10 个落在 C，剩下 40 个两三个一组散着。前三是答案；散着的那些是噪声，而排序正是把它们丢掉的那一步。

**展开成整个社区。** 如果结果要的是社区本身、而不是被采样的节点，每个入选簇就是一次 `0x0B` 前缀扫描，它的成员行经登记册还原成元组——每次调用一次登记册扫描，覆盖这次调用的整个成员集：

```rust
let hyperedges = Hyperedges::of::<Employment>(&store);
let communities: Vec<Vec<EdgeKey>> = top
    .iter()
    .map(|cluster| hyperedges.members(*cluster))
    .collect::<Result<_, _>>()?;
```

之后由应用去格式化——`## 社区 A — 100 个召回节点中的 30 个` 这样的 markdown 记忆块，后面跟上展开的成员——因为那是呈现，而 okm 不做呈现。

**什么是策略、什么是机制。** 取几个簇、平局怎么破、要不要展开成成员、一页是 100 还是 20：全是应用的决定，没有一个需要新面。okm 只提供让这件事便宜的那两个动词——节点形态的 `clusters_of`（一次 walk 加每条关联边一次成员查询，即线性于这一页的边数）与 `members`（一次前缀扫加一次登记册扫）。另一种形状——遍历每个簇、数有多少召回节点落在其中——为了回答关于 100 个节点的问题，要去读整张 `0x0A` 和 `0x0B`。

**单成员的社区能存，空簇也能存。** 簇是一个作用域，而作用域本身就是信息：检测出只有一条边的组是真实答案，空簇则是「这个主题敏感，别问」「这里曾经有东西，现在没了」的表达方式。成员变化永远不会删掉簇行——簇在声明时创建、只在显式删除时删除（`.delete(..)`），所以一个把成员全部丢光的社区会作为空作用域留下，计数诚实地写着零。所以上面故事里的"散着"是呈现问题，不是存储的限制：一个在某个主题下什么都没找到的检测器，照样能记下"我找过"。

**一次运行自己的事实住在哪。** 一次召回运行有它自己的元数据——查询、页大小、时间戳、跑的是哪个检测器——那不是社区。它是一条父簇：运行声明一个自己 kind 的簇（`Kind::EVENT`，长期主题则用 `SCOPE`），每个社区用 `.under(运行号)` 声明在它下面，`children(kind, 运行号)` 枚举这次运行的全部社区，而父簇自己的载荷装运行的事实。父簇不需要自己的成员——一条成员面为空、社区作为子簇挂在下面的父簇，正是这套结构要支撑的形状。

## 两种形态，一个查询面

`Graph<S, E>`（固定本体：属性是声明 struct 字段，body 取类型化字节）与 `okm_dynamic::Graph<S>`（属性是运行时 nTLV 帧，`DynEdge { predicate: String, attrs: Dict }`）在相同的面上暴露相同的读方法。查询可见的差异：

- `get_edge` 返回原始 body（`EdgeBody.payload` = 载荷字节），`edge` 再把它解成声明的类型；对比解码后的 `DynEdge`（属性经字段名词汇表还原）。
- 动态形态没有声明的属性面（属性名是运行时数据，等值属性扫描结构性缺席——0x08/0x09 的谓词过滤是完整的，不是缺口）。
- 节点侧的 "kind" 不是边层的关切：节点是普通文档 collection，其 kind 活在自己声明的索引与词汇表里。`(:node_type)` 过滤 = 边面扫描 ∩ 节点 collection 自己的 kind 面扫描，内存交集。

什么不该建为图——按信任域定能合不拆、型与态分置（实例态留在 collection）、跨图经 NodeRef 组合——是建模指南的裁量，不在本文：见 [MODELING.zh-CN.md](MODELING.zh-CN.md)「什么该进图」。