# ADR 目录

架构决策记录。每篇一个决策，记录真实理由（不做技术体面话包装）。

- [0001 — 边方向位 niche 进命名空间字段最高位](0001-direction-bit-niche.md)（已实现）
- [0002 — 命名空间字典留在代码、永不落 KV](0002-namespace-dictionary.md)（已实现；「多租户」草图由 [0010](0010-virtual-storage-remote-bytes.md) 提升为正式机制）
- [0003 — Collection 是唯一组装点，无 KvRecord 宏](0003-collection-assembly-point.md)（已实现，含 save_into 跨集合原子路径）
- [0004 — Value 侧：版本化 payload、TLV 扩展区与字段 wrapper](0004-value-side-and-wrappers.md)（机制有效；宏层面被 [0006](0006-row-node-model.md) 取代——`ValueEncode` 取消，规则并入 `ObjEncode` payload 半边）
- [0005 — 二级索引：item 内 slot 自动编号，索引不占手动 ns](0005-secondary-index-slots.md)（已实现；挂载点由 [0006](0006-row-node-model.md) 移到行结构体）
- [0006 — Row/Node 模型：单宏声明行，ValueEncode 并入，Node 与 Edge 平级](0006-row-node-model.md)（已实现）
- [0007 — Arrow/DataFrame 桥：行 → RecordBatch，快照分层](0007-arrow-dataframe-bridge.md)（已实现：eager 桥 + Parquet 快照；lazy pushdown 载入门控）— [中文](0007-arrow-dataframe-bridge.zh-CN.md)
- [0008 — 事件层：reduce 改名、subscribe 通道、okm-stream](0008-event-layer-reduce-subscribe-stream.md)（已实现：bare `#[ok_subscribe]`、批次 epoch、okm-stream）— [中文](0008-event-layer-reduce-subscribe-stream.zh-CN.md)
- [0009 — FRP 对照：Spacetimedb 订阅 vs okm-stream](0009-frp-comparison-subscriptions-stream.md)（分析；无实现义务）— [中文](0009-frp-comparison-subscriptions-stream.zh-CN.md)
- [0010 — VirtualStorage：KvEngine 即存储边界，远端字节走既有通道](0010-virtual-storage-remote-bytes.md)（已接受，实现待做——PLAN Phase 7）— [中文](0010-virtual-storage-remote-bytes.zh-CN.md)
- [0011 — 存储完整键：不剥前缀](0011-full-keys-in-storage.md)（已接受）— [中文](0011-full-keys-in-storage.zh-CN.md)
- [0012 — Object 模式：单一编码 + 字段名字典 + ok_ 改名](0012-object-model-and-field-dictionary.md)（设计定案，实现待排期）— [中文](0012-object-model-and-field-dictionary.zh-CN.md)
- [0015 — Junction 改名与两类关系载体](0015-junction-rename-and-relation-types.md)（已接受；其中 2 字节 slot 段表由 0016 取代）— [中文](0015-junction-rename-and-relation-types.zh-CN.md)
- [0016 — 4 字节 entry 头与段编号 slot](0016-four-byte-head-and-slot-segments.md)（已接受）— [中文](0016-four-byte-head-and-slot-segments.zh-CN.md)
- [0017 — Graph Edge：第三类关系载体](0017-graph-edge.md)（已接受；两种形态均已实现）— [中文](0017-graph-edge.zh-CN.md)
- [0018 — 存储值就是引擎自己的值模型，序列化文本绝不作为存储表示](0018-storage-values-native-not-serialized-text.md)（已接受；aura 侧已落地 2026-09-22——`StoreAsVirtual` 删除 + state 文档化）— [中文](0018-storage-values-native-not-serialized-text.zh-CN.md)
- [0019 — 部分索引：声明上的 where(path) 谓词，而不是返回 Option 的函数](0019-partial-index-where-predicate.md)（已实现——两种索引形态都可用，谓词不进 entry 地址）— [中文](0019-partial-index-where-predicate.zh-CN.md)
- [0020 — 范围扫描是引擎唯一的有序读原语——trait 边界上的惰性迭代](0020-range-scan-lazy-iterator.md)（已接受；引擎层已实现——Collection 层惰性镜像与文档随同批次）— [中文](0020-range-scan-lazy-iterator.zh-CN.md)
- [0021 — wire 层的流式扫描——远端路径加入惰性契约](0021-remote-streaming-scan.md)（已落地——OP_SCAN_STREAM 分页 + 自足 chunk + 惰性 Remote 臂，扫描/流应答为前缀相对键）— [中文](0021-remote-streaming-scan.zh-CN.md)
- [0022 — bindings 语义对齐——动态模式的能力上限是有范围的，不是永久的](0022-bindings-semantic-alignment.md)（已接受——callable 本地执行、远程携带语义结果；subscribe 仍排除）— [中文](0022-bindings-semantic-alignment.zh-CN.md)
- [0023 — 预置 reduce 组合子：Count/Max/Min/Sum 作为库声明](0023-preset-reduce-combinators.md)（已接受——一行声明、零样板；不做内置表统计；MaxKeep 独立命名）— [中文](0023-preset-reduce-combinators.zh-CN.md)
- [0024 — reduce 钩子接收解码后的 key：fold(acc, key, item)](0024-reduce-hooks-receive-the-key.md)（已接受——key 以解码引用直达钩子、GROUP 双源命名；镜像字段模式退役）— [中文](0024-reduce-hooks-receive-the-key.zh-CN.md)

实施计划见 [docs/PLAN.md](../PLAN.md)。
