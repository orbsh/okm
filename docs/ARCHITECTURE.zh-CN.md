# ARCHITECTURE — 双层声明机器

OKM 是 KV 对标 ORM 的范式。本文解释它的中心机制：一个 Rust struct 声明如何变成 KV 字节，以及实现为什么拆成现在这样。规范性建模指南见[建模指南](MODELING.zh-CN.md)；设计决策见 [docs/adr/](adr/)。（英文版为主文档：[ARCHITECTURE.md](ARCHITECTURE.md)。）

## 双层：编译期展开 + 常量表

OKM 的每个特性都跨两层，这个拆分是刻意的：

**第一层——derive 展开（热路径）**。derive 宏（`KeyEncode`、`DocumentEncode`、`JunctionEncode`）把 struct 声明展开成**直接的字节操作代码**——`extend_from_slice(&self.field.to_be_bytes())` 序列，没有中间表示、没有运行期 schema 遍历、没有反射。这是快路径：put/get 的编解码以纯指针算术的速度运行，与手写编解码器产出的代码相同。

**第二层——常量表（其余一切）**。与编码代码一起，derive 向类型上发射**关联常量**——`PAYLOAD_FIELDS`（每字段的名字与宽度）、`FIELDS`（带类型的字段描述符）、`HOT_WIDTH`、`NS_PREFIX`、每个访问方法的 `SLOT`、`DEFAULTS`、`FIELD_CONTRACTS`。这些常量是**反射面**：一切非热路径的东西读它们，而不是重新推导声明。

```text
#[derive(DocumentEncode)]
#[ok_ns(9)]
#[ok_index(by_a { fields(x) })]
struct User { x: u32, name: String }
        │ derive
        ▼
第一层（代码）                      第二层（常量）
encode_payload()                   PAYLOAD_FIELDS: [("x",4),("name",0)]
decode_payload()                   FIELDS: [FieldDesc; 2]
index_entries()                    HOT_WIDTH: 4
__okm_encode_named()               NS_PREFIX: [0,9]
        │                          by_a::SLOT: 0x1001
        ▼                          DEFAULTS / FIELD_CONTRACTS
Collection::put/get/scan ───────── 两层在此汇合
        │
VirtualStorage (Fjall / redb / slatedb)
```

**为什么这个拆分重要**：声明的消费者——schema 导出（`TableSchema::of`）、动态 reader（okm-dynamic）、Arrow/Parquet 桥、快照工具——需要的是 schema 的**知识**，不是它的**代码**。它们读常量。加一个消费者永远不需要碰 derive；加一个字段只需要 derive 知道多发射一条常量项。两层独立扩展，仅由 `Document` trait 的关联项耦合。

这也是动态模式能存在的原因：`TableSchema`（从常量组装而来）是 schema 的完整、可序列化描述——Python 与 Steel 绑定从它构建 collection，全程不出现任何 Rust 类型。

## 模块布局

```text
okm-core/src/
├── model/    面向声明的核心——key 编解码、Document trait 与 Collection、
│             索引、junction、reduce、schema 导出、动态段编解码、
│             包装类型（wrappers/）
├── engine/   KV 引擎边界——VirtualStorage 抽象及其适配器
│             （Fjall / redb / slatedb）、TestStore、远程嵌套。
│             引擎专属的东西全部在此模块之后；model 层只见 trait。
├── bridge/   对外导出——Arrow/Parquet 列式桥、JSON-schema/快照工具。
│             只读第二层常量。
└── subscribe 写路径事件发射（跨切面，留顶层）
```

依赖方向单向：`model` 永远不点名引擎；`bridge` 永远不做计算——它读常量并格式化。

## key 语法

所有 key 共享一套头部纪律：

```text
[ (0xFF part 1B) ][ ns 2B BE ][ slot 2B BE ][ 身份 / 字段 ... ]
```

- **ns**（u16 BE）：集合的身份，在 document 上声明一次（`#[ok_ns]`），16 位全空间由所有条目种类共享。
- **slot**（u16 BE）：4 bit 段号（条目种类——文档自身 / 索引 / reduce / junction）+ 12 bit 段内计数。段归属是结构事实（`slot >> 12`），移位即分派，不是约定（ADR-0016）。
- **身份尾段**：永远是主键编码（全长或声明的截断），所以每条派生条目都可逆回它的源文档。

junction 是唯一的跨集合条目：每个端点 ns 一条单向条目（双 ns 寄生，ADR-0015），方向由承载条目的 ns 决定，区分号在 slot 低位。

## wire 纪律

所有变长框架使用同一套编解码——前缀单调 varint（`wrappers/wire.rs`，P1/P2）：字节比较等于数值比较，所以 `VarInt<T>` 合法进入索引段，且每条 TLV 长度前缀（动态段帧、声明冷段帧、Vector 元素 LV）只花 1–2 字节而非 4。定宽的东西保持定宽：hot 段字段与索引 key 永不变长编码，因为静态偏移与排序序是让快路径快的契约。

## derive 不做什么

- 无 I/O。每个生成的函数都是纯字节进字节出。
- 无引擎知识。`Collection<S, K, R>` 在拼装点绑定引擎；derive 无法命名一个引擎。
- 无运行期注册表。"声明即注册表"——索引 slot、合同、默认值都是常量，编译期读取或一次性组装进 `TableSchema`。

## 后续方向

PLAN 追踪开放工作（Set 类型——倒排索引，KDL schema 序列化——JSON 的低优先级替代）。ADR 系列记录了上述每个决策为什么如此——ADR-0002（ns 字典）、ADR-0005（索引 slot）、ADR-0012（对象模型 + 字典）、ADR-0015/0016（关系 + 4 字节头）。
