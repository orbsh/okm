# Key 布局：ns 前缀与层级判别

本文是机制与实现细节文档：所有落进 engine 的 key 的第一级布局——2 字节 ns 前缀怎么编码、文档与 junction 怎么共用这一级、slot 怎么接在后面。slot 的分配纪律与 append-only 契约见[slot 机制](slot-mechanism.zh-CN.md)；决策记录见 [ADR-0002](../adr/0002-namespace-dictionary.md)、[ADR-0005](../adr/0005-secondary-index-slots.md)、[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.zh-CN.md)（4 字节头与段号制 slot，现行的分配）。

## 声明位置：ns 挂在文档上，不挂在 Key 上

`#[ok_ns(N)]` 声明在 **document struct** 上，不在 key struct 上：

- document 是集合的声明点——它的 `#[ok_ref]` 已把 key 类型钉死，`Collection<S, K, R>` 三个参数由 document 完全决定，ns 是这个集合的身份的一部分；
- key 类型不带 ns，意味着**同一个 key 形状可以合法服务多个 document / 多个集合**，各挂各的 ns 号。若 ns 挂 key，这个场景被编译期堵死，只能靠多声明一份同构 key 类型绕行。

derive（`okm-derive`）把 `#[ok_ns(N)]` 编译成 `Document::NS_PREFIX: &'static [u8]`（大端 `[hi, lo]` 两字节，未声明 = 空切片，对应仅做 codec、不落表的 document）。`Collection::new(store)` 不收 ns 参数——拼装点只选 engine，不复述 ns（ns 字典是代码，ADR-0002；engine 选择是每拼装点的自由，ADR-0010）。

junction 不声明 ns：它的字段引用**文档类型**（`user: User`），derive 反查 `<User as Document>::NS_PREFIX` 取端点 ns（ADR-0015、ADR-0016）。ns 只在文档上声明一次。

## 第一级：2 字节 ns 前缀，所有条目共用

所有条目的 key 都以同构的 2 字节 BE 头开头——ns 原值大端写入，文档、索引、reduce、junction 共用完整的 16 位编号空间，无变换、无保留半区：

```text
[ ns 2B BE ] ...
```

## 第二级：2 字节 slot（4 bit 段号 + 12 bit 计数）

ns 头之后是 2 字节 BE slot：高 4 位是**段号**（条目种类的结构分派，`slot >> 12`），低 12 位是段内计数。完整段表与决策理由见 [ADR-0016](../adr/0016-four-byte-head-and-slot-segments.zh-CN.md)；现行分配：

```text
段 0x0  文档自身     slot 0x0000 主表          [ns][0x0000][主键编码]      value = TLV payload
                     slot 0x0001 obj 动态段     [ns][0x0001][主键编码]      value = nTLV 帧
                     slot 0x0002 字段名字典     [ns][0x0002][field-id]     → 名字
                     slot 0x0003 字段名字典     [ns][0x0003][名字]         → field-id
                     slot 0x0004+  缓冲（4092 个）
段 0x1  声明索引     [ns][0x1nnn][索引字段][主键前缀]   nnn = 声明序
段 0x2  reduce       [ns][0x2nnn][group 段]            nnn = 声明序，独立计数器
段 0x3  junction     [ns][0x3nnn][对端身份]            nnn = junction 区分号
段 0x4–0xB 预留（派生/关系扩展）
段 0xC–0xF 预留（系统）
```

段号是**枚举**（条目种类），不是空间分配：16 个种类对应"文档自身 / 几类派生 / 关系 / 系统"的量级；计数位 4096/段，宽于旧扁平 256。junction 在每个端点的 ns 里各写一条单向 entry（双 ns 寄生，ADR-0015），方向由条目所在 ns 承载，不由 slot 位承载——不再有正向/反向 slot 对。

slot 段号让"集合内加派生数据"永不侵占相邻集合的 ns 段（ADR-0005）；段归属是结构事实（移位即得类别），不是编号纪律。分配纪律、append-only 契约、变长字段的约束见[slot 机制](slot-mechanism.zh-CN.md)。

**partition 前缀（可选，在 ns 头之前）**：`#[ok_partition(N)]` 声明的集合，所有键在最前面多一段 `[0xFF][N 1B]`：

```text
partition 条目  [ 0xFF ][ part 1B ][ ns 2B ][ slot 2B ][ ... ]
```

`0xFF` 是转义字节——合法 ns 头（大端 u16，首字节受 ns 字典纪律约束为 0x00–0xFE）永不以它开头，所以 partition 集合与普通集合的键空间**结构性不相交**，无需任何编号协调（partition(0) 非法——直接省略属性即无段）。语义：partition 是 workload 隔离（compaction 分组），不是所有权边界；ns 仍是归属的最外层。

## 全景

```text
document  [ (0xFF part 1B) ][ ns 2B ][ slot 2B ][ ... ]   partition 段可选
junction  [ ns_a 2B         ][ 0x3nnn ][ 对端身份 ]            在 A 的集合里
junction  [ ns_b 2B         ][ 0x3nnn ][ 对端身份 ]            在 B 的集合里
            ↑ 同一编号空间、同一头纪律，slot 段号区分条目种类——无变换
```

## reduce 的位置

reduce 条目没有独立 ns——它寄生在宿主文档的 ns 段里，段 0x2，计数器独立于索引（ADR-0016 取消了旧的续接耦合）：

```text
reduce 条目  [ ns 2B ][ 0x2nnn ][ group 段 ]   value = acc 编码
```

对扫描而言，一个 ns 段内前缀 `[ns][slot]` 完整枚举了这张文档的所有派生状态：段 0 主表/动态段/字典、段 1 索引、段 2 reduce、段 3 junction。机制见[reduce 机制](reduce-mechanism.zh-CN.md)。
