# ADR-0016：4 字节条目头与段号制 slot

日期：2026-09-18
状态：已接受
取代：ADR-0005（1 字节 slot）、ADR-0011（edge slot 14/15）、ADR-0012（扁平固定角色 slot 表）中的 slot 分配部分。junction 寄生决策见 ADR-0015。

## 背景

所有派生条目（索引、reduce、junction）的 key 都以 `[ns 2B][slot 1B]` 开头。1 字节 slot 是逐代生长出来的：固定角色占 0–15，声明的索引/reduce 从 16 续接，edge 钉死在 14/15（ADR-0011），条目**种类**之间没有任何结构性隔离——段归属只是纪律约定，slot 字节本身不携带"这是什么种类的条目"的信息。

容量配比也与消费者错位：u8 slot 给每个角色同样的 256 预算，而真实消耗极不对称（固定角色屈指可数，声明索引随声明增长，junction 按关系成对）。

## 决策

条目头加宽到 4 字节——**ns u16 + slot u16，均大端**——slot 内部结构化拆分：

```text
slot u16 = [段号 4 bit][计数 12 bit]

段号（高 nibble）：
0x0  文档自身（计数：0 主表、1 动态、2/3 字典、4–4095 缓冲）
0x1  声明索引（计数 = 声明序）
0x2  reduce（计数 = 声明序，独立——不再续接索引计数器）
0x3  junction（计数 = junction 区分号，声明式）
0x4–0xB  预留（派生/关系扩展）
0xC–0xF  预留（系统）
```

- **段提取是结构操作**：`slot >> 12`。原本的数值区间约定变成类型化分派——把某段的 slot 用到另一种条目上是布局错误，不是纪律失误。
- **计数宽度变大**：每段 4096 个条目，宽于旧扁平 256。单个集合的声明索引容量从 240 到 4096；junction 每对端点 4096 个区分号。
- **16 个段号作为枚举足够**：段是条目**种类**的枚举，不是空间分配。耗尽 16 个种类意味着语义革命，本来就该走版本迁移。
- 实现中整个头是一个 u32：`((ns as u32) << 16) | slot`。扫描前缀构造与段分派都是移位，无查表。
- 非主键条目 key 增加一个字节。相对多字节身份载荷不可见；主键（段 0x0 计数 0）所在文档自身 key 形状不变。

## Junction 落位（配合 ADR-0015）

junction 在**每个端点集合自己的 ns** 里写一条单向条目——共两条，无第三个 ns：

```text
[ns_a 2B][slot 0x3nnn][B 身份]   在 A 的集合里
[ns_b 2B][slot 0x3nnn][A 身份]   在 B 的集合里
```

`nnn` 是 junction 区分号（`#[ok_junction(n)]`），区分同一端点对上的多条 junction。方向由条目所在的 ns 承载，不由 slot 位承载——取代 ADR-0011 的 forward/reverse slot 对（14/15），后者存在的原因只是两个方向挤在一个 ns 里。

junction 声明引用**文档类型，不是 key 类型**：

```rust
#[derive(JunctionEncode)]
#[ok_junction(1)]
struct Membership {
    user: User,
    org: Org,
}
```

derive 解析 `<User as Document>::Key` 做身份编码、`<User as Document>::NS_PREFIX` 取端点 ns。ns 只在文档上声明一次，junction 不复述任何东西。

## 后果

- **key 格式破坏性变更**：所有非主键条目 key 增加一个字节；全部 hex 锁测试更新。零成本窗口：未发布，无外部数据。
- reduce 不再续接索引计数器——两种声明各自独立配额。
- `SlotMap`（schema 导出）字段变 u16，`edge_fwd`/`edge_rev` 合并为单个 `junction`。
- `KvIndex::SLOT` / `KvJunction` 的 slot 类型加宽为 u16。
