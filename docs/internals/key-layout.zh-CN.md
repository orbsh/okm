# Key 布局：ns 前缀与 slot 分派

本文是机制与实现细节文档：所有落进 engine 的 key 的第一级布局——2 字节 ns 前缀怎么编码、文档与 junction 怎么共用这一级、slot 怎么接在后面。slot 的分配纪律与 append-only 契约见[slot 机制](slot-mechanism.zh-CN.md)；决策记录见 [ADR-0002](../adr/0002-namespace-dictionary.md)、[ADR-0005](../adr/0005-secondary-index-slots.md)、[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.zh-CN.md)（条目头，其固定面部分已被取代）、[ADR-0030](../adr/0030-slot-fixed-faces-and-counting-families.zh-CN.md)——现行分配：固定面一字节，计数族保留 2 字节 slot。

## 声明位置：ns 挂在文档上，不挂在 Key 上

`#[ok_ns(N)]` 声明在 **document struct** 上，不在 key struct 上：

- document 是集合的声明点——它的 `#[ok_ref]` 已把 key 类型钉死，`Collection<S, K, R>` 三个参数由 document 完全决定，ns 是这个集合的身份的一部分；
- key 类型不带 ns，意味着**同一个 key 形状可以合法服务多个 document / 多个集合**，各挂各的 ns 号。若 ns 挂 key，这个场景被编译期堵死，只能靠多声明一份同构 key 类型绕行。

derive（`okm-derive`）把 `#[ok_ns(N)]` 编译成 `Document::NS_PREFIX: &'static [u8]`（大端 `[hi, lo]` 两字节，未声明 = 空切片，对应仅做 codec、不落表的 document）。`Collection::new(store)` 不收 ns 参数——拼装点只选 engine，不复述 ns（ns 词汇表是代码，ADR-0002；engine 选择是每拼装点的自由，ADR-0010）。

junction 不声明 ns：它的字段引用**文档类型**（`user: User`），derive 反查 `<User as Document>::NS_PREFIX` 取端点 ns（ADR-0015、ADR-0016）。ns 只在文档上声明一次。

## 第一级：2 字节 ns 前缀，所有条目共用

所有条目的 key 都以同构的 2 字节 BE 头开头——ns 原值大端写入，文档、索引、reduce、junction 共用完整的 16 位编号空间，无变换、无保留半区：

```text
[ ns 2B BE ] ...
```

## 第二级：slot——固定面一字节，计数族两字节

ns 头之后是 slot。它的首字节自己说明后面还跟不跟计数字节：

```text
固定面        首字节高半字节 = 0     slot = 1 字节   [ns][0x0F]
计数族        首字节高半字节 ≠ 0     slot = 2 字节   [ns][0x1n][nn]
                                                    计数 = (n << 8) | nn
```

**固定面**（1 字节，16 个号，现用 12 个）——一族只有一张面，没有可枚举的东西。大端是"高半字节在左"的原因，也是这条定界成立的原因：0x00–0x0F 永远不可能等于计数族的首字节（≥ 0x10）。

```text
0x00  主表              [ns][0x00][主键]                             value = TLV payload
0x01  obj 动态段        [ns][0x01][主键]                             value = nTLV 帧
0x02  字段名词汇表      [ns][0x02][field-id]                         → 名字
0x03  字段名词汇表      [ns][0x03][名字]                             → field-id
0x04  谓词词汇表        [ns][0x04][predicate_id]                     → 谓词名
0x05  谓词词汇表        [ns][0x05][谓词名]                           → predicate_id
0x06  图 谓词索引       [ns][0x06][predicate_id][edge_id]
0x07  图 出边           [ns][0x07][src NodeRef][edge_id]
0x08  图 入边           [ns][0x08][dst NodeRef][edge_id]
0x09  图 谓词+出        [ns][0x09][predicate_id][src 引用][edge_id]
0x0A  图 谓词+入        [ns][0x0A][predicate_id][dst 引用][edge_id]
0x0B  向量索引快照      [ns][0x0B][shard u16 BE]  → 建好的图（okm-vector 缓存，可丢）
0x0C–0x0F                   留给将来的固定面
（表按角色排：两本词汇表在前，读它们编号的图面在后——词汇表排在它服务的面之前；编号才是机制。）
```

**计数族**（2 字节，12 位计数，号码不变）——族内实例按声明序或区分号枚举：

```text
0x1nnn  声明索引     [ns][0x1nnn][索引字段][主键前缀]   nnn = 声明序
0x2nnn  reduce       [ns][0x2nnn][group 段]             nnn = 声明序，独立计数器
0x3nnn  junction     [ns][0x3nnn][本端身份][对端身份]    nnn = 区分号 << 1 | 方向
段 0x4–0x0F          空着
```

宽度规则是关于字节的事实（`b & 0xF0 == 0` 读 1 字节 slot，否则读 2 字节），不是查一张要维护的表——与下文的 ns 转义字节、以及自描述端点引用（ADR-0017 §2）同一立场。所以按族拼前缀扫描天然精确，不需要查任何东西：`[ns][0x07]` 不可能命中计数族，`[ns][0x1n][nn]` 不可能命中固定面。

每族的计数容量是 4096：声明索引与 reduce 占 1–4095（0 保留），junction 把整个范围用于 `区分号 << 1 | 方向`（每端点对 2048 个区分号）。junction 在每个端点集合自己的 ns 里写一条单向条目（双 ns 寄生，ADR-0015）；方向住在计数的低位——只有自反 junction（两端同 ns）结构上需要它，否则两个方向会共用一条扫描前缀。

slot 让"集合内加派生数据"永不侵占相邻集合的 ns 段（ADR-0005），而且它的首字节在两种形态下都是结构事实：既界定 slot 又给出族名，固定面号不可能被误读成族号。被取代的段表（所有条目统一的 4 bit 段号 + 12 bit 计数）见 [ADR-0016](../adr/0016-four-byte-head-and-slot-segments.zh-CN.md)。分配纪律、append-only 契约、变长字段的约束见[slot 机制](slot-mechanism.zh-CN.md)。

**partition 前缀（可选，在 ns 头之前）**：`#[ok_partition(N)]` 声明的集合，所有键在最前面多一段 `[0xFF][N 1B]`：

```text
partition 条目  [ 0xFF ][ part 1B ][ ns 2B ][ slot … ][ ... ]
```

`0xFF` 是转义字节——合法 ns 头（大端 u16，首字节受 ns 词汇表纪律约束为 0x00–0xFE）永不以它开头，所以 partition 集合与普通集合的键空间**结构性不相交**，无需任何编号协调（partition(0) 非法——直接省略属性即无段）。语义：partition 是 workload 隔离（compaction 分组），不是所有权边界；ns 仍是归属的最外层。

## 全景

```text
document  [ (0xFF part 1B) ][ ns 2B ][ slot 1B（固定面）   ][ ... ]
document  [ (0xFF part 1B) ][ ns 2B ][ slot 2B（计数族）   ][ ... ]
junction  [ ns_a 2B         ][ 0x3nnn ][ 本端身份 ][ 对端身份 ]   在 A 的集合里
junction  [ ns_b 2B         ][ 0x3nnn ][ 本端身份 ][ 对端身份 ]   在 B 的集合里
            ↑ 同一编号空间、同一头纪律；slot 首字节既界定宽度也给出族名——无变换
```

## reduce 的位置

reduce 条目没有独立 ns——它寄生在宿主文档的 ns 段里，段 0x2，计数器独立于索引（ADR-0016 取消了旧的续接耦合）：

```text
reduce 条目  [ ns 2B ][ 0x2nnn ][ group 段 ]   value = acc 编码
```

对扫描而言，一个 ns 段内的前缀完整枚举了这张文档的所有派生状态：固定面 0x00–0x03（主表、动态段、字段名词汇表两个方向），然后是计数族 0x1 索引、0x2 reduce、0x3 junction。图（边集合）另加它自己的固定面：谓词词汇表 0x04/0x05（它的谓词）与 0x06–0x0A（谓词索引、出入边、谓词±端点）。带 kind token 的文档集合声明同一对词汇表——同一条规则，谁需要谁用。机制见[reduce 机制](reduce-mechanism.zh-CN.md)。