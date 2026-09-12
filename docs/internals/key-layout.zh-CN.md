# Key 布局：ns 前缀与层级判别

本文是机制与实现细节文档：所有落进 engine 的 key 的第一级布局——2 字节 ns 前缀怎么编码、table 与 edge 怎么共用这一级、方向位 niche 在哪、slot 怎么接在后面。slot 的分配纪律与 append-only 契约见[slot 机制](slot-mechanism.zh-CN.md)；决策记录见 [ADR-0001](../adr/0001-direction-bit-niche.md)、[ADR-0002](../adr/0002-namespace-dictionary.md)、[ADR-0005](../adr/0005-secondary-index-slots.md)。

## 声明位置：ns 挂在 Row 上，不挂在 Key 上

`#[kv_ns(N)]` 声明在 **row struct**（或 edge struct）上，不在 key struct 上：

- row 是表的声明点——它的 `#[kv_ref]` 已把 key 类型钉死，`Table<S, K, R>` 三个参数由 row 完全决定，ns 是这张表的身份的一部分；
- key 类型不带 ns，意味着**同一个 key 形状可以合法服务多个 row / 多张表**，各挂各的 ns 号。若 ns 挂 key，这个场景被编译期堵死，只能靠多声明一份同构 key 类型绕行。

derive（`okm-derive`）把 `#[kv_ns(N)]` 编译成 `Row::NS_PREFIX: &'static [u8]`（大端 `[hi, lo]` 两字节，未声明 = 空切片，对应仅做 codec、不落表的 row）。`Table::new(store)` 不收 ns 参数——拼装点只选 engine，不复述 ns（ns 字典是代码，ADR-0002；engine 选择是每拼装点的自由，ADR-0010）。

## 第一级：2 字节 ns 前缀，table 与 edge 共用

所有条目的 key 都以同构的 2 字节 BE 头开头，table（row 表）与 edge 共用同一个 ns 编号空间：

```text
[ ns 2B BE ] ...
  └ table  : ns 占满 16 位，原值大端写入
  └ edge   : ns 占低 15 位，最高位是方向位（FWD=0, REV=1）
```

edge 的头是 `(ns << 1 | dir).to_be_bytes()`（`okm-core/src/edge.rs` 的 `head_bytes`）——方向位 niche 进 ns 字段的最高位（ADR-0001），没有独立的第三字节。代价是 edge 可用的 ns 号只有 0..=32767（声明类型仍是 u16）；收益是头保持定宽 2 字节，前缀扫描可以把头当固定 tag 用。

方向语义：

```text
edge ns=4，user → session

forward key  [head(ns=4, dir=0)][A·identity][B·identity]   正向：起点身份在前
reverse key  [head(ns=4, dir=1)][B·identity][A·identity]   反向：终点身份在前
```

高位为 1 的段即反向段——双写保证两个方向都在（`EdgeTable::link` 一次写两条），扫描"某节点的所有邻居" = 同一 ns 的 FWD + REV 两段前缀扫描拼接。

## 第二级：table 后面接 1 字节 slot

table 侧，ns 头之后是 1 字节 slot，0 是主表（`PRIMARY_SLOT`），1 起按 `#[kv_index]` 声明序分配给访问方法：

```text
主表条目  [ ns 2B ][ slot=0 ][ 主键编码 ]                 value = TLV payload
索引条目  [ ns 2B ][ slot≥1 ][ 索引字段 ][ 主键前缀 ]      value = includes TLV
```

slot 字节让"表内加索引"永不侵占相邻表的 ns 段（ADR-0005）；分配纪律、append-only 契约、变长字段的约束见[slot 机制](slot-mechanism.zh-CN.md)。

edge 侧没有第二级——头之后直接是端点身份编码，正向/反向的分岔已由方向位承担，不存在"一条边多个访问方法"的形态。

## 全景

```text
table  [ ns 2B            ][ slot 1B    ][ ... ]
edge   [ ns<<1|dir 2B BE   ][ A·id ][ B·id ]     （反向时 A/B 对调）
         ↑ 共用同一编号空间：table 用满 16 位，edge 用低 15 位 + 最高位方向位
```

注意一个工程后果：ns 字典手动分配时，**table 号与 edge 号混在同一空间里**——`#[kv_ns(4)]` 既是 row 表也是 edge 的合法号，互撞由字典纪律（append-only、人工分配，ADR-0002）而不是类型系统阻止。.ns 号 32768..=65535 对 edge 不可表达（方向位 niche 的直接后果），分配时 table 可用的号比 edge 宽一倍。

## reduce 的位置

reduce 条目没有独立 ns——它寄生在宿主 row 的 ns 段里，slot 续接该 row 的索引计数器（最后一个索引 slot + 1 起，同样 append-only）：

```text
reduce 条目  [ ns 2B ][ slot≥N ][ group 段 ]   value = acc 编码
```

对扫描而言，一个 ns 段内前缀 `[ns][slot]` 完整枚举了这张表的所有派生状态：slot 0 主表、1.. 索引与 reduce。机制见[reduce 机制](reduce-mechanism.zh-CN.md)。
