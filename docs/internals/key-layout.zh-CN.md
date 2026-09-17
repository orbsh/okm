# Key 布局：ns 前缀与层级判别

本文是机制与实现细节文档：所有落进 engine 的 key 的第一级布局——2 字节 ns 前缀怎么编码、table 与 edge 怎么共用这一级、方向位 niche 在哪、slot 怎么接在后面。slot 的分配纪律与 append-only 契约见[slot 机制](slot-mechanism.zh-CN.md)；决策记录见 [ADR-0001](../adr/0001-direction-bit-niche.md)、[ADR-0002](../adr/0002-namespace-dictionary.md)、[ADR-0005](../adr/0005-secondary-index-slots.md)。

## 声明位置：ns 挂在 Row 上，不挂在 Key 上

`#[ok_ns(N)]` 声明在 **row struct**（或 edge struct）上，不在 key struct 上：

- row 是表的声明点——它的 `#[ok_ref]` 已把 key 类型钉死，`Table<S, K, R>` 三个参数由 row 完全决定，ns 是这张表的身份的一部分；
- key 类型不带 ns，意味着**同一个 key 形状可以合法服务多个 row / 多张表**，各挂各的 ns 号。若 ns 挂 key，这个场景被编译期堵死，只能靠多声明一份同构 key 类型绕行。

derive（`okm-derive`）把 `#[ok_ns(N)]` 编译成 `Row::NS_PREFIX: &'static [u8]`（大端 `[hi, lo]` 两字节，未声明 = 空切片，对应仅做 codec、不落表的 row）。`Table::new(store)` 不收 ns 参数——拼装点只选 engine，不复述 ns（ns 字典是代码，ADR-0002；engine 选择是每拼装点的自由，ADR-0010）。

## 第一级：2 字节 ns 前缀，table 与 edge 共用

所有条目的 key 都以同构的 2 字节 BE 头开头——ns 原值大端写入，table 与 edge 共用完整的 16 位编号空间，无变换、无保留半区：

```text
[ ns 2B BE ] ...
```

## 第二级：1 字节 slot

ns 头之后是 1 字节 slot。完整分配（终态）：

```text
slot 0      主表                [ns][0][主键编码]                 value = TLV payload
slot 1      obj 动态段          [ns][1][主键编码]                 value = nTLV 帧
slot 2      字段名字典          [ns][2][field-id]                → 名字
slot 3      字段名字典          [ns][3][名字]                    → field-id
slot 4–13   保留（两端向中间增长的缓冲带）
slot 14     edge 正向           [ns][14][A·identity][B·identity]
slot 15     edge 反向           [ns][15][B·identity][A·identity]
slot 16+    index / reduce      [ns][slot≥16][索引字段/组段][主键前缀]
```

分配呈**两端固定、向中间收敛**的形态（类堆栈内存布局）：固定角色从 0 向上（主表、动态段、字典，未来的新固定角色按出现顺序向上领号），edge 从顶端向下（15 反向、14 正向），中间 4–13 是未划分的自由缓冲——不做内部区域划分，谁需要谁领号，两侧相遇即耗尽。

- 主表与派生：0 主表（`PRIMARY_SLOT`），1 obj 动态段（ADR-0012），2–3 字段名字典（双向），16 起按 `#[ok_index]` 声明序分配访问方法、reduce 续接同一计数器。
- edge（PLAN Phase 10，取代 ADR-0001 方向位 niche）：正/反各占一个 slot（14/15），头就是 ns 原值——不再有 `ns<<1|dir` 变换，table 与 edge 布局完全同构，16 位 ns 全宽对两者开放。双写保证两个方向都在（`EdgeTable::link` 一次写两条），扫描"某节点的所有邻居" = slot 14 + slot 15 两段前缀扫描拼接。

slot 字节让"表内加派生数据"永不侵占相邻表的 ns 段（ADR-0005）；分配纪律、append-only 契约、变长字段的约束见[slot 机制](slot-mechanism.zh-CN.md)。

**partition 前缀（可选，在 ns 头之前）**：`#[ok_partition(N)]` 声明的表，所有键在最前面多一段 `[0xFF][N 1B]`：

```text
partition 表条目  [ 0xFF ][ part 1B ][ ns 2B ][ slot ][ ... ]
```

`0xFF` 是转义字节——合法 ns 头（大端 u16，首字节受 ns 字典纪律约束为 0x00–0xFE）永不以它开头，所以 partition 表与普通表的键空间**结构性不相交**，无需任何编号协调（partition(0) 非法——直接省略属性即无段）。语义：partition 是 workload 隔离（compaction 分组），不是所有权边界；ns 仍是归属的最外层。

## 全景

```text
table  [ (0xFF part 1B) ][ ns 2B ][ slot 1B ][ ... ]   partition 段可选
edge   [ ns 2B          ][ slot 6|7 ][ A·id ][ B·id ]
         ↑ 同一编号空间、同一头纪律，slot 区分派生数据——无方向位变换
```

注意一个工程后果：ns 字典手动分配时，**table 号与 edge 号在同一空间里是真共享**——`#[ok_ns(4)]` 下 table 的 slot 0 是主表，edge 的 slot 6/7 是边，两者共存于同一 ns 段，互不冲突也不需要错开编号。

## reduce 的位置

reduce 条目没有独立 ns——它寄生在宿主 obj 的 ns 段里，slot 从 16 起与索引共用计数器（按声明序分配，append-only）：

```text
reduce 条目  [ ns 2B ][ slot≥16 ][ group 段 ]   value = acc 编码
```

对扫描而言，一个 ns 段内前缀 `[ns][slot]` 完整枚举了这张 obj 的所有派生状态：0 主表、1 动态段、2–3 字典、14–15 边、16+ 索引与 reduce。机制见[reduce 机制](reduce-mechanism.zh-CN.md)。
