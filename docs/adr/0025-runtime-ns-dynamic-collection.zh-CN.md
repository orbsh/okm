# ADR-0025: 运行时 ns 的动态 collection——运行期组装的键控文档

> **Languages:** [English](0025-runtime-ns-dynamic-collection.md)（主文档）· [中文](0025-runtime-ns-dynamic-collection.zh-CN.md)

**Status:** Accepted (2026-09-24); implementation follows

## Context

`Collection<S, K, R>` 在编译期绑定 namespace：`R::NS_PREFIX` 是 derive 生成的关联常量，`K: KeyEncode` 是编译期 DDL 结构体（ADR-0002：ns 字典即代码；key derive 对任何变宽 key 字段直接 panic）。对声明的表这两个绑定都是正确的——写程序时 schema 已知。

aura 的 ADR-0026（类型级 actor 存储）需要 ns 是**运行时值**的 collection：每个注册的 actor 类型从注册表分配一个真实 ns（`ACTOR_NS_BASE + id`），宿主在类型上传之前不可能知道这些编号。应用在 `interface_schema` 携带的 schema 里声明自己的 collection；存储层必须能对着类型已分配的 ns 具现它们——既不能每个 ns 编译一个表类型（不可能，ns 是数据），也不能退回单表文本前缀方案（那会重新引入类型 ns 裁决要消除的共享键空间混叠）。

第二个要求是 key。actor 声明的 collection 用应用字段给文档做键（如 `{user: "alice", seq: 3}`），所以 key 和 ns 一样是运行时数据。引擎契约仍然要求键字节有序：`scan_range` 是**唯一**的有序读原语（VirtualStorage），索引与前缀语义都骑在字节序上，每个定宽 `KeyEncode` 字段用大端正是为了让编码序等于值序。

## Decision

### 1. `DynamicCollection<S>`——运行时 ns 的组装点

`okm-core` 新增 collection 类型（`model/dynamic_collection.rs`）：

```rust
pub struct DynamicCollection<S> { /* ns: [u8; 2], store: S, dict: DictCache */ }
impl<S: VirtualStorage> DynamicCollection<S> {
    pub fn new(store: S, ns: u16) -> Self;
    pub fn put(&mut self, key: &[(String, DynamicValue)], doc: &BTreeMap<String, DynamicValue>);
    pub fn get(&mut self, key: &[(String, DynamicValue)]) -> Option<BTreeMap<String, DynamicValue>>;
    pub fn delete(&mut self, key: &[(String, DynamicValue)]) -> bool;
    pub fn scan_prefix(&mut self, prefix: &[(String, DynamicValue)], limit: Option<u64>) -> Vec<Doc>;
    pub fn scan_range(&mut self, begin/end over the same key frame, limit) -> Vec<Doc>;
}
```

key 是**有序的**具名 `DynamicValue` 字段列表（不是 map——顺序定义编码，名字经字段名字典解析，与动态段同一套，ADR-0012）。key 帧用自己的保序 wire；document **值**复用既有的 nTLV 动态段帧与字典，不改。

### 2. 动态 key wire 保序

每字段字节序必须等于值序，否则 scan/range 语义失真。nTLV 值帧不能直接用（`len varint` 前缀破坏字典序）。key 帧对每个字段编码为：

- `UInt(u64)` / `Bool`——定宽大端（字节序即值序）。
- `Int(i64)`——大端二补数异或符号位（标准保序变换：翻最高位后按无符号比较）。
- `F64`——IEEE 大端加 sign-magnitude → magnitude-sign 重映射（标准浮点全序键）。
- `Str` / `Bytes`——**终结符编码**：原始字节，`0x00` 转义为 `0x00 0xFF`，以单个 `0x00` 结尾（标准字典序；文本字段上的前缀扫描行为正确）。
- `Null`——空字段（同类型字段中排最前）；`Array` / `Obj` **拒绝**作为 key 字段（其顺序语义是调用方的问题——先拍平）。

每个 key 字段携带 `[id u16 BE]` 前缀 + 保序 body。id 前缀保持自描述寻址（ADR-0011）且保序稳定（字典 id 对单个 collection 稳定）。

### 3. 布局：与声明表同一套 header 纪律

键为 `[ns 2B][slot][key frame]`，slot 常量与声明表完全相同（primary `0x00 0x00`、动态段 `0x00 0x01`、字典 `0x00 0x02/0x00 0x03`）——运行时 ns 恰好处在编译期 `NS_PREFIX` 的位置，remote 帧、nest 前缀、宿主字节布局全部保持一致（ADR-0011 全键、ADR-0010 wire）。字段名字典住在 collection 自己的 ns 段内，与声明表相同。

不与 partition 段交互：运行时 ns 的 collection 是无分区的（`PARTITION_PREFIX` 为空）；actor 数据上的负载隔离需求尚未实证。

### 4. 范围守卫：DynamicCollection 不做什么

无 derive、无声明 payload 字段、无类型化解码、无 `#[ok_index]`/`#[ok_reduce]` 声明（schema 即声明；二级索引是调用方自己的辅助 collection）、无 subscribe 事件。它是宿主组装 schema 的逃生舱——声明式表仍是默认，运行时 ns 的 collection 只在 ns 本身成为数据时使用。

## Honest semantic cost

- **字典引入跨机词汇依赖**：id 只在单个引擎的字典内稳定；节点间导出原始 key 字节必须连同字典（它确实在同一 ns 段内——但消费方必须知道这一点）。
- **key 字段类型窄于值帧**：无 Array/Obj 键，`Int`/`F64` 需要保序变换（同一抽象值现在存在两种编码：值 TLV 与 key wire）。接受：键序是让扫描诚实的性质；变换是标准且可测的。
- **key 形状无编译期校验**：两个写入方对同一 collection 用不同字段顺序会无声地分叉键空间。schema 声明（调用方契约）是防线。

## Consequences

- aura 的 ADR-0026 executor 把每个 actor 声明的 collection 具现为类型注册分配 ns 上的 `DynamicCollection`——每类型一个真实 ns，无文本前缀混叠。
- `Collection`（编译期）仍是主接口面；声明表的任何行为不变。
- 动态 key wire 是新 wire 面：hex 布局稳定性测试套件补齐保序变换用例（Int 符号位、F64 重映射、Str 终结符转义）。
