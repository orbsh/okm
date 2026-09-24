# ADR-0025: 两个 schema 载体、一个键空间——静态代码生成与动态 schema 数据模式

> **Languages:** [English](0025-runtime-ns-dynamic-collection.md)（主文档）· [中文](0025-runtime-ns-dynamic-collection.zh-CN.md)

**Status:** Accepted (2026-09-24) — 架构记录；机制先于本 ADR 存在

> 文件名说明：本 ADR 初稿题为「运行时 ns 动态 collection」，提议在 okm-core 新增一种 collection 类型；初稿同日**撤回**——okm-dynamic 的 `DynamicCollection`（运行时 ns、schema 类型化 key）已覆盖该需求，而 okm-core 中的第二套保序 key wire 会破坏本 ADR 记录的字节相等契约。ADR 以双模式架构记录的形态保留。

## Context

aura 的 ADR-0026（类型级 actor 存储）引出一个问题：当 namespace 是**运行时值**时，宿主应使用 okm 的哪个接口面——每个注册的 actor 类型从注册表分配一个真实 ns，应用在 upload 时携带 schema 声明自己的 collection。出现过两个候选答案：

1. 在 okm-core 新增一种 collection 类型，key 是运行时字段帧（保序 wire、解码时携带 `KeyKind` 清单）。
2. **既有的** okm-dynamic `DynamicCollection`：运行时 ns 构造器，key 按 `CollectionSchema` 值编码——与 derive 产出相同的定宽 BE 纪律。

方案 1 是重复造轮子且退化：动态模式早已存在（okm-dynamic，ADR-0022 谱系——schema 驱动编解码、bindings、共享 plan 执行核），而第二套 key wire 会使字节级契约分叉。这场混乱暴露的问题值得记录：动态模式住在哪里、两个 schema 载体如何关联。

## Decision

### 1. 一个引擎契约、一套键布局、两个 schema 载体

- **静态模式——代码生成。** Rust 类型 + okm-derive；编译器即 schema 校验器（键宽、偏移、索引声明是编译期事实）；组装点是 `Collection<S, K, R>`。面向 Rust 宿主与 Rust 源码编译的 wasm actor。
- **动态模式——schema 即数据。** 一个 `CollectionSchema` 值（静态侧经 `CollectionSchema::of` 导出，或以数据形式编写）+ okm-dynamic。`DynamicCollection` 接受运行时 ns 与 schema 值；编解码与 derive 逐字节一致。面向嵌入式语言 actor（Python/Steel bindings）与运行时组装 collection 的宿主。

对同一声明，两个载体产生**完全相同的字节**：同一 header 纪律、同一键编码、同一 payload 帧、同一字典行为。一方写的数据另一方读得回来——**模式是写入方的属性，不是数据的属性**。外部 API 刻意对齐（put/get/scan/delete + document map），应用代码形状不分叉。

### 2. 动态模式住在 okm-dynamic，不在 okm-core

okm-core 宿主**共享运行时**：引擎契约（`VirtualStorage`）、编译期模式组装点（`Collection`）、document/动态段机制、reduce、subscribe。okm-dynamic 宿主 schema 驱动编解码与 `DynamicCollection`——包括其运行时 ns 构造器（ADR-0022 谱系即已存在；core 从不需要改动）。okm-derive 是静态代码生成；okm-wire 是远程 wire 格式；okm-query/stream/graph/vector/ngram 是 core 之上的消费侧算子。只被动态模式使用的键空间原语不进 core。

### 3. aura 消费形态（ADR-0026）

- **python / steel actor**：`ctx.store.emit(op)` → realm 解析类型注册分配的 ns（`meta::ns_of`），经 okm-dynamic `DynamicCollection`（按 actor 声明的 schema 构建）执行 op。
- **wasm（Rust 源码）actor**：静态路径编译**进模块**——derive + `Collection`，脚本在宿主桥之上实现 `VirtualStorage`，引擎调用跨桥执行。完全体：索引、reduce、编译期校验；无 schema 数据绕行。
- 两者在同一 ns 写相同字节，数据跨载体语言可互操作。

## Honest semantic cost

- **两个载体、一个漂移面。** 字节相等是承重墙，由跨语言测试锁定；derive 改动不同步动态编解码（或反之）会无声分叉布局。bindings 的帧字节相等验收测试是防线。
- **动态模式的 schema 是运行时数据**——键宽、索引声明无编译期校验；畸形 schema 在执行时以编解码错误浮现，而非构建期。

## Consequences

- 撤回的 okm-core 初稿已移除；okm-dynamic 仍是动态模式的唯一家园。README 在根文档记录双模式分工。
- aura 的 executor 工作在 okm-dynamic 之上推进；ADR-0026 不需要任何 okm-core 改动。
