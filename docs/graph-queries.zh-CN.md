# 图查询配方：六个面如何组合

图边层（ADR-0017）的查询配方：六个查询入口面如何组合成 Cypher 熟悉的形状，`NodeRef` 是什么、为什么自描述，过滤发生在哪一层。wire 布局与写路径见[建模指南](MODELING.zh-CN.md)，机制记录见 [ADR-0017](adr/0017-graph-edge.zh-CN.md)。

## 核心立场

一个面 = 一个访问方法；图查询 = 先扫最 selective 的面、在内存收敛 id——没有组合面，没有查询引擎。所有原语返回 `Vec<u64>`（edge id）；只有幸存者才逐 id 取 body。声明即执行，与 OKM 全库一致。

## NodeRef：自描述的节点引用

`NodeRef` 是图边的端点地址——`[ns 2B][len varint][pkey]`：

```text
┌─────────┬───────────┬──────────────────┐
│ ns  2B  │ len varint│  pkey（裸）       │
└─────────┴───────────┴──────────────────┘
   类型       宽度随行        身份
  标记
```

- **`ns` 是类型标记。** 一张图连接多个 collection 的节点；节点属于哪个 collection 不是声明出来的，是 ref 里的两个字节。ns 字节与 `Document::NS_PREFIX` 同形（`[ns 2B BE]` 头）——节点引用由源文档自己的头加 key 编码拼装：`NodeRef::new(&<Org as Document>::NS_PREFIX, &key.encode())`。
- **`len` 住在 ref 里。** 不同节点 collection 可声明不同 pkey 宽度（4 字节组织 id、16 字节 agent id）且共存。解码按 varint 切出 pkey——无注册表、无编译期端点绑定。这就是端点开放的机制：任何文档 collection 零仪式参与。
- **同节点同字节。** 编码是规范的，遍历面精确前缀匹配，混合宽度的节点不会互撞。

`NodeRef` 与 `EdgeBody` 配对——一条边的 body 读取：

```rust
pub struct EdgeBody {
    pub src: NodeRef,
    pub dst: NodeRef,
    pub kind_id: u16,     // kind 名经 g.kind_name(kind_id) 还原
    pub attrs: Vec<u8>,   // 边自己的载荷（固定本体形态：EdgeEncode 载荷；
                          // 动态形态：nTLV 帧）
}
```

多跳遍历就是拿 `EdgeBody.dst` 当下一跳的 `NodeRef`。

## 六个面，作为查询形状

前置：`Org(ns=9)` 与 `User(ns=10)` 上的 `employs` 图，写入两条边（组织 9 -> 用户 1，2020 年；组织 9 -> 用户 2，2021 年）。

```text
Cypher 形状                          okm 落点（面）
──────────────────────────────────────────────────────────
(:org {id:9})-[]->()                out_edges          (0x5)
()<-[]-(:user {id:1})               in_edges           (0x6)
()-[r:employs]->()                  edges_of_kind      (0x4)
(:org {id:9})-[r:employs]->()       typed_out          (0x7)
()-[r:employs]->(:user {id:1})      typed_in           (0x8)
()-[r:employs {since: 2020}]->()    by_attr_face       (0x1)
```

```rust
// 无类型邻接——触碰节点的全部边，不限 kind。
g.out_edges(&org9);                  // -> [1, 2]
g.in_edges(&user1);                  // -> [1]

// kind 限定遍历——kind 领衔扫描前缀（0x7/0x8）。
g.typed_out("employs", &org9);       // -> [1, 2]
g.typed_in("employs", &user1);       // -> [1]

// 全图 kind 扫描——`MATCH ()-[r:employs]->()` 的入口。
g.edges_of_kind("employs");          // -> [1, 2]

// 属性面——边事实的声明字段，等值探针。
let slot = __OkmEdgeIndex_Employment_since_year::SLOT;
g.by_attr_face(slot, &2020u16.to_be_bytes());  // -> [1]
g.by_attr_face(slot, &1999u16.to_be_bytes());  // -> []（无面无错）

// body 读取——端点 + kind 名 + 属性，只对幸存 id。
let body = g.get_edge(1).unwrap();
let kind = g.kind_name(body.kind_id).unwrap(); // -> "employs"
```

所有原语返回 edge id；扫不到返回空 Vec——未知 kind 与不存在的属性值都是空结果，不是错误。

## 组合：内存交集，无组合面

这些面刻意正交。单个面覆盖不了的谓词（kind AND 端点类型 AND 属性）是若干面扫描的合取，在内存按 edge id 交集：

```rust
// 组织 9 的雇佣边、2020 年入职、雇员是 User（ns 10）——三个面，最小结果集优先：
let candidates: Vec<u64> = g.by_attr_face(slot, &2020u16.to_be_bytes())
    .into_iter()
    .filter(|id| g.typed_out("employs", &org9).contains(id))
    .filter(|id| g.get_edge(*id).unwrap().dst.ns_id() == 10)
    .collect();
```

成本纪律与 OKM 全库一致：先扫最 selective 的面（等值属性匹配 > kind 扫描 > 无类型邻接），让字节区间承担物理 where——内存交集只对幸存者付费。某个合取变热时，把它提升为边 collection 上的访问方法（对边自己的声明属性字段建 `#[ok_index]`），而不是在查询期组合——声明即注册，查询路径坍缩为一次前缀扫。

## 多跳：id 与节点的分工

原语返回 **edge id**——身份货币（平行边、`get_edge`、`unlink` 都挂在它上面）。遍历货币是 **`NodeRef`**。两者的桥是邻居家族——面扫描 + 回取的便捷组合（零新 wire）：

```rust
// 一跳，只要节点：
g.out_nodes(&user1);                    // -> [org(9), org(10)]
g.in_nodes(&org9);                      // -> [user(1)]
g.typed_out_nodes("employs", &user1);   // kind 限定邻居
g.typed_in_nodes("employs", &org9);

// 平行边保留重数（同一对节点两条事实产出两次同一邻居）；集合语义是调用方的去重：
let mut uniq = g.out_nodes(&user1);
uniq.sort();
uniq.dedup();
```

多跳本身是广度优先 walk——frontier + visited 集——visited 集按 `NodeRef` 记（它 derive `Hash` + `Ord` 正是为此）。重访节点是 KV 侧花掉无界扫描预算的方式：防环不是可选结构，就是成本模型。每跳 = 每方向每边类型一次前缀扫；成本线性于触碰的边数，与图的大小无关。

```rust
// 雇佣关系的朋友圈形状：组织 -> 用户 -> 组织，两跳。
let mut frontier = vec![org9];
let mut visited: std::collections::HashSet<NodeRef> = frontier.iter().cloned().collect();
for _ in 0..2 {
    let mut next = Vec::new();
    for n in &frontier {
        for m in g.out_nodes(n) {
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

walk 成为热路径时，把边 collection 的热谓词提升为声明的 `#[ok_index]`（见组合一节）——walk 的每跳扫描已经是物理 where，剩下的内存工作只有 visited 集。

## 两种形态，一个查询面

`Graph<S, E>`（固定本体：属性是声明 struct 字段，body 取类型化字节）与 `DynamicGraph<S>`（属性是运行时 nTLV 帧，`DynEdge { kind: String, attrs: BTreeMap<..> }`）在相同的面上暴露相同的读方法。查询可见的差异：

- `get_edge` 返回类型化载荷（`EdgeBody.attrs` = 边类型字节）vs 解码后的 `DynEdge`（属性经字典还原）。
- 动态形态没有 0x1 属性面（属性名是运行时数据，等值属性扫描结构性缺席——0x4/0x7/0x8 的 kind 过滤是完整的，不是缺口）。
- 节点侧的 "kind" 不是边层的关切：节点是普通文档 collection，其 kind 活在自己声明的索引与字典里。`(:node_type)` 过滤 = 边面扫描 ∩ 节点 collection 自己的 kind 面扫描，内存交集。
