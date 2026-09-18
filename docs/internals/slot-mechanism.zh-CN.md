# Slot 机制：派生条目的命名空间内分派

本文是机制与实现细节文档：slot 如何把派生条目分派进 2 字节命名空间、约束是什么、为什么这样设计。建模纪律见[建模指南](../MODELING.zh-CN.md)；决策记录见 [ADR-0005](../adr/0005-secondary-index-slots.md)、[ADR-0016](../adr/0016-four-byte-head-and-slot-segments.zh-CN.md)（4 bit 段号 + 12 bit 计数，现行分配）。

## 条目布局

每张集合的条目分两类，靠 ns 段区分：

```text
主表条目  [ ns 2B BE ][ slot 0x0000 ][ 主键 ]                  value = TLV payload
索引条目  [ ns 2B BE ][ slot 0x1nnn ][ 索引字段 ][ 主键前缀 ]    value = includes TLV
```

判别符 = 2 字节命名空间 + 2 字节 slot：ns 段划整个集合，slot 的段号（高 4 位）在**集合段内部**结构化区分条目种类。全层级 key 布局见[key 布局](key-layout.zh-CN.md)；本篇只展开 slot 这一级。`#[ok_ns]` 回归"一个集合一个号"的最清洁语义——集合的 ns 分配与派生条目数量彻底无关（ADR-0005）。

## slot 分配

`SLOT` 是编译期常量：段号固定（索引 = 0x1），低 12 位计数按 `#[ok_index]` 在 document struct 上的**声明序**分配：第 1 条 = `0x1001`，第 2 条 = `0x1002`，……主表占 `0x0000`（`PRIMARY_SLOT`）。

```text
#[ok_ns(9)]
struct User { ... }
#[ok_index(by_a ...)]   →  SLOT=0x1001，entry 头 = [0,9,0x10,0x01]
#[ok_index(by_b ...)]   →  SLOT=0x1002，entry 头 = [0,9,0x10,0x02]
```

reduce 走段 0x2、junction 走段 0x3，各自独立的计数器（ADR-0016 取消了 reduce 续接索引计数器的旧耦合）。表间隔离由 ns 字典（ADR-0002）手动分配保证；slot 只负责**集合内**隔离。单集合上限 4096 个索引（12 bit 计数）——超限是编译期事实（序号溢出），不是运行期风险。

## 编译期锁定的东西

泛型参数 `I: KvIndex` 在编译期给扫描路径带来三样静态信息，运行期零查找：

- `SLOT`：扫描前缀的第 3–4 字节（`[ns 2B][slot 2B]` 头）；
- 索引字段段宽度：`encode_named` 按声明序把 payload 字段编码成 BE 字节，宽度由 `KeyEncode`/字段类型算术决定；
- 主键前缀宽度：`key_prefix_width()`——`KEY_PREFIX` 为空时取 `KEY_LEN`（全长主键），截断时取命名子集的累计宽度。

运行期只做三件事：拼前缀（`entry_prefix`）、引擎范围扫（`scan_suffix`）、按已知宽度从条目 key 尾段切出主键编码并解码。

## Append-only 纪律

SLOT 依赖声明序意味着**索引声明序列是持久化契约**：

- 只能尾部追加。中途插入会让其后所有索引的 slot 漂移；已落库条目留在旧 slot 位，`scan::<I>` 换了前缀后返回空结果——静默错误。
- **不允许直接删除声明**——位置编号下删除中间声明会让其后所有 slot 前移，新写入落到前一位声明的存量条目上（静默损坏，与被否决的加法推导同类）。下线一条索引的正确路径是 `deprecated` 标记：`#[ok_index(by_old { … }, deprecated)]` 保留 slot 占位、不生成任何写入/扫描面；历史存量条目由 `Collection::prune_deprecated_slots()` 显式清除（按 `[ns][deprecated slot]` 前缀扫删，幂等，返回删除数）。
- 无索引回填：尾部追加的新索引只对之后写入的文档生效；存量文档不补条目，需要覆盖时走迁移双写。

重排等价于换布局：清库重建或迁移双写，没有原地改序的路径。

## 历史

原始设计（ADR-0005 正文）权衡过三种形态：4-bit 位拼接（`ns << 4 | slot`，ns 空间砍半）、1 字节 slot 字节（当时选中）、以及后来实现时一度采用的**加法推导**（`index_ns = table_ns + SLOT`，省掉 slot 字节）。加法推导的致命缺陷：表的 ns 分配不再自洽——分配 ns=256 时必须为"未来会加几个索引"预留段余量，加索引可能撞进下一张表的段，且错误静默（两张表的段重叠后 scan 照常返回，只是读到对方条目）。2026-09-10 回到 1 字节 slot 字节形态。

2026-09-18 slot 加宽为 2 字节并引入段号制（ADR-0016）：条目种类从编号约定升为结构分派（4 bit 段号），计数位扩到 4096/段；junction 落段 0x3（双 ns 寄生，ADR-0015），取代 ADR-0011 的 14/15 正反向 slot 对。
