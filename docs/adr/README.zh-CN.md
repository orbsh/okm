# ADR 目录

架构决策记录。每篇一个决策，记录真实理由（不做技术体面话包装）。

- [0001 — 边方向位 niche 进命名空间字段最高位](0001-direction-bit-niche.md)（已实现）
- [0002 — 命名空间词汇表留在代码、永不落 KV](0002-namespace-dictionary.md)（已实现；「多租户」草图由 [0010](0010-virtual-storage-remote-bytes.md) 提升为正式机制）
- [0003 — Collection 是唯一组装点，无 KvRecord 宏](0003-collection-assembly-point.md)（已实现，含 save_into 跨集合原子路径）
- [0004 — Value 侧：版本化 payload、TLV 扩展区与字段 wrapper](0004-value-side-and-wrappers.md)（机制有效；宏层面被 [0006](0006-row-node-model.md) 取代——`ValueEncode` 取消，规则并入 `ObjEncode` payload 半边）
- [0005 — 二级索引：item 内 slot 自动编号，索引不占手动 ns](0005-secondary-index-slots.md)（已实现；挂载点由 [0006](0006-row-node-model.md) 移到行结构体）
- [0006 — Row/Node 模型：单宏声明行，ValueEncode 并入，Node 与 Edge 平级](0006-row-node-model.md)（已实现）
- [0007 — Arrow/DataFrame 桥：行 → RecordBatch，快照分层](0007-arrow-dataframe-bridge.md)（已实现：eager 桥 + Parquet 快照；lazy pushdown 载入门控）— [中文](0007-arrow-dataframe-bridge.zh-CN.md)
- [0008 — 事件层：reduce、subscribe、okm-stream](0008-event-layer-reduce-subscribe-stream.md)（已实现：bare `#[ok_subscribe]`、批次 epoch、okm-stream）— [中文](0008-event-layer-reduce-subscribe-stream.zh-CN.md)
- [0009 — FRP 对照：Spacetimedb 订阅 vs okm-stream](0009-frp-comparison-subscriptions-stream.md)（分析；无实现义务）— [中文](0009-frp-comparison-subscriptions-stream.zh-CN.md)
- [0010 — VirtualStorage：KvEngine 即存储边界，远端字节走既有通道](0010-virtual-storage-remote-bytes.md)（已接受，实现待做——PLAN Phase 7）— [中文](0010-virtual-storage-remote-bytes.zh-CN.md)
- [0011 — 存储完整键：不剥前缀](0011-full-keys-in-storage.md)（已接受）— [中文](0011-full-keys-in-storage.zh-CN.md)
- [0012 — Object 模式：单一编码 + 字段名词汇表 + ok_ 改名](0012-object-model-and-field-dictionary.md)（设计定案，实现待排期）— [中文](0012-object-model-and-field-dictionary.zh-CN.md)
- [0015 — Junction 改名与两类关系载体](0015-junction-rename-and-relation-types.md)（已接受；其中 2 字节 slot 段表由 0016 取代）— [中文](0015-junction-rename-and-relation-types.zh-CN.md)
- [0016 — 4 字节 entry 头与段编号 slot](0016-four-byte-head-and-slot-segments.md)（已接受；段表由 [0030](0030-slot-fixed-faces-and-counting-families.md) 取代）— [中文](0016-four-byte-head-and-slot-segments.zh-CN.md)
- [0017 — Graph Edge：第三类关系载体](0017-graph-edge.md)（已接受；两种形态均已实现）— [中文](0017-graph-edge.zh-CN.md)
- [0018 — 存储值就是引擎自己的值模型，序列化文本绝不作为存储表示](0018-storage-values-native-not-serialized-text.md)（已接受；aura 侧已落地 2026-09-22——`StoreAsVirtual` 删除 + state 文档化）— [中文](0018-storage-values-native-not-serialized-text.zh-CN.md)
- [0019 — 部分索引：声明上的 where(path) 谓词，而不是返回 Option 的函数](0019-partial-index-where-predicate.md)（已实现——两种索引形态都可用，谓词不进 entry 地址）— [中文](0019-partial-index-where-predicate.zh-CN.md)
- [0020 — 范围扫描是引擎唯一的有序读原语——trait 边界上的惰性迭代](0020-range-scan-lazy-iterator.md)（已接受；引擎层已实现——Collection 层惰性镜像与文档随同批次）— [中文](0020-range-scan-lazy-iterator.zh-CN.md)
- [0021 — wire 层的流式扫描——远端路径加入惰性契约](0021-remote-streaming-scan.md)（已落地——OP_SCAN_STREAM 分页 + 自足 chunk + 惰性 Remote 臂，扫描/流应答为前缀相对键）— [中文](0021-remote-streaming-scan.zh-CN.md)
- [0022 — bindings 语义对齐——动态模式的能力上限是有范围的，不是永久的](0022-bindings-semantic-alignment.md)（已接受——callable 本地执行、远程携带语义结果；subscribe 仍排除）— [中文](0022-bindings-semantic-alignment.zh-CN.md)
- [0023 — 预置 reduce 组合子：Count/Max/Min/Sum 作为库声明](0023-preset-reduce-combinators.md)（已接受——一行声明、零样板；不做内置表统计；MaxKeep 独立命名）— [中文](0023-preset-reduce-combinators.zh-CN.md)
- [0024 — reduce 钩子接收解码后的 key：fold(acc, key, item)](0024-reduce-hooks-receive-the-key.md)（已接受——key 以解码引用直达钩子、GROUP 双源命名；镜像字段模式退役）— [中文](0024-reduce-hooks-receive-the-key.zh-CN.md)
- [0025 — 双模式载体共用一个键空间：静态 codegen 与动态 schema 数据](0025-runtime-ns-dynamic-collection.md)（已接受——字节恒等的两种写入模式；模式是写入方的属性，不是数据的）— [中文](0025-runtime-ns-dynamic-collection.zh-CN.md)
- [0026 — VirtualStorage 写操作取 &self：共享句柄契约一次说清](0026-virtualstorage-shared-handle-writes.md)（已接受，实现待做——trait 放宽 + 消费者清扫）— [中文](0026-virtualstorage-shared-handle-writes.zh-CN.md)
- [0027 — VirtualStorage 接口审计：派生便利方法退出 trait](0027-virtualstorage-surface-audit.md)（已接受并实施——scan_suffix_kv 与 batch() 出 trait、KvBatch 删除、异步面按审计后形态对齐）— [中文](0027-virtualstorage-surface-audit.zh-CN.md)
- [0028 — 浏览器内的 VirtualStorage 家族：localStorage 引擎 + 异步线协议客户端](0028-in-browser-virtualstorage.md)（已接受并实现）— [中文](0028-in-browser-virtualstorage.zh-CN.md)
- [0029 — 路径索引：在无 schema 区上声明的路径](0029-path-index.md)（已接受并实现——Path 组件词汇 + 自定界无计数编码 + 一路径一 slot；挂在动态段写路径上）— [中文](0029-path-index.zh-CN.md)
- [0030 — slot：固定面一字节，计数族两字节](0030-slot-fixed-faces-and-counting-families.md)（已接受并落地——单张面的族各占一个 1 字节号 0x00–0x0A（含图的一对谓词词汇表 0x04/0x05）；计数族保留 2 字节 slot 与 12 位计数，号码与上限不变）— [中文](0030-slot-fixed-faces-and-counting-families.zh-CN.md)
- [0031 — 批量写路径承载语义：BatchStore 适配器](0031-batch-write-semantics.md)（已接受并落地——点读穿透 batch、scan 大声拒绝；`save_with_prev_into` 让跨表原子提交与 reduce 并存，旧面在同一批内删除）— [中文](0031-batch-write-semantics.zh-CN.md)
- [0032 — 分类值是数据：自带两个族的 `Vocab<N>`](0032-vocab-categorical-values.md)（已接受并落地——`Enum<T>`/`EnumTag` 退役；每个 `Vocab<N>` 字段占 0x1nnn/0x2nnn 两方向，分配游标是 id 面的面级单值（裸前缀，不占面）；索引 0x3nnn、reduce 0x4nnn、junction 0x5nnn；`N` = u8…u128 定 id 宽度，超限大声报错，intern 走写路径且可在批内完成）— [中文](0032-vocab-categorical-values.zh-CN.md)
- [0033 — slot 留在 ns 之后，面的元数据留在面上](0033-slot-order-and-face-metadata.md)（已接受——迭代由前缀端点界定、无需逐键边界校验；数据与元数据按后缀长度分离，代价每条一次比较；游标不单开族也不搬 keyspace；slot 前置会让 ns 偏移浮动、挤走 0xFF 转义、把 collection 的连续区间按面切碎）— [中文](0033-slot-order-and-face-metadata.zh-CN.md)
- [0034 — 表名与导出的形状](0034-table-names-and-export-shape.md)（已接受并落地——`#[ok_name]` → `TABLE_NAME`、边集合导出成一张表、reduce 表单独导出、图边属性面归 `Graph` 自己）— [中文](0034-table-names-and-export-shape.zh-CN.md)
- [0035 — 整文档只有一个接口面：view / merge / replace / purge](0035-whole-document-api.md)（已接受并落地——静态段说结构体、整文档说一个名字键 map；每次写一个引擎提交；六个按槽切分的旧名退役，无别名）— [中文](0035-whole-document-api.zh-CN.md)
- [0036 — 字段太大，放进副表](0036-value-too-big-is-its-own-table.md)（拒绝 `#[ok_offrow]`——复杂度大、功能提升小，且与 ADR-0035 的整文档面语义冲突；替代是独立 ns、同主键的副表，配对写仍是一次提交）— [中文](0036-value-too-big-is-its-own-table.zh-CN.md)
- [0037 — 面向 map 的写动词返回 `Result`](0037-write-verb-error-surface.md)（已接受——`okm_core::Error`（thiserror）：调用方数据用 `KindMismatch`/`ValueOutOfRange`，引擎拒绝用 `Engine`；`from_map` 改 `try_from_map`、`merge`/`replace` 返回 `Result`；typed 动词维持 panic 契约，拓宽是单独一遍）— [中文](0037-write-verb-error-surface.zh-CN.md)

实施计划见 [docs/PLAN.md](../PLAN.md)。
