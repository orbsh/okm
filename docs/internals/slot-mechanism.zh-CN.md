# Slot 机制：索引命名空间的推导

本文是机制与实现细节文档：slot 如何把索引条目分配进 2 字节命名空间、约束是什么、为什么这样设计。建模纪律见[建模指南](../MODELING.zh-CN.md)；决策记录见 [ADR-0005](../adr/0005-secondary-index-slots.md)。

## 条目布局

每张表的条目分两类，靠 ns 段区分：

```text
主表条目  [ table_ns 2B BE ][ 主键 ]              value = TLV payload      (slot 0)
索引条目  [ index_ns 2B BE ][ 索引字段 ][ 主键前缀 ]  value = includes TLV
```

2 字节命名空间是唯一判别符——条目头里没有 slot 字节。每个索引通过自己的 `index_ns` 获得独立 ns 段，主表条目与各索引条目天然隔离。

## ns 推导

`index_ns = table_ns + SLOT`（`okm/src/index.rs:180`），其中 `SLOT` 是编译期常量，按 `#[kv_index]` 在行 struct 上的**声明序**分配：第 1 条 = 1，第 2 条 = 2，……主表占 0（`PRIMARY_SLOT`）。

```text
#[kv_ns(9)]
struct User { ... }
#[kv_index(by_a ...)]   →  SLOT=1, index_ns=10
#[kv_index(by_b ...)]   →  SLOT=2, index_ns=11
```

推导是 `wrapping_add`。ns 段的全局唯一性不由 slot 保证——不同表的 `table_ns + SLOT` 可能撞进同一段，表间隔离由 ns 字典（ADR-0002）手动分配保证；slot 只负责**表内**隔离（每个索引一个独立 ns 段）。

## 编译期锁定的东西

泛型参数 `I: KvIndex` 在编译期给扫描路径带来三样静态信息，运行期零查找：

- `SLOT`（→ `index_ns`）：扫描前缀的头 2 字节；
- 索引字段段宽度：`encode_named` 按声明序把 payload 字段编码成 BE 字节，宽度由 `KeyEncode`/字段类型算术决定；
- 主键前缀宽度：`key_prefix_width()`——`KEY_PREFIX` 为空时取 `KEY_LEN`（全长主键），截断时取命名子集的累计宽度。

运行期只做三件事：拼前缀（`entry_prefix`）、引擎范围扫（`scan_suffix`）、按已知宽度从条目 key 尾段切出主键编码并解码。

## Append-only 纪律

SLOT 依赖声明序意味着**索引声明序列是持久化契约**：

- 只能尾部追加。中途插入会让其后所有索引的 `index_ns` 漂移；已落库条目留在旧 ns 段，`scan::<I>` 换了前缀后返回空结果——静默错误。
- 删除声明 = 留下 ns 洞，无害，永不回收（与 ns 编号同一纪律，ADR-0002）。
- 无索引回填：尾部追加的新索引只对之后写入的行生效；存量行不补条目，需要覆盖时走迁移双写。

重排等价于换布局：清库重建或迁移双写，没有原地改序的路径。

## 历史

原始设计（ADR-0005 正文）是表 ns 段内一个 1 字节 slot 字节（`[table_ns 2B][slot 1B][fields]`），支持单表 256 索引且 ns 空间不受挤占。2026-09-07 实现时简化为当前形态：**slot 机制保留在 ns 推导里，slot 字节从条目布局中移除**——2 字节表命名空间已足够区分所有 entry，省一个字节、少一层判别。
