# ADR 目录

架构决策记录。每篇一个决策，记录真实理由（不做技术体面话包装）。

- [0001 — 边方向位 niche 进命名空间字段最高位](0001-direction-bit-niche.md)（已实现）
- [0002 — 命名空间字典留在代码、永不落 KV](0002-namespace-dictionary.md)（已实现；「多租户」草图由 [0010](0010-virtual-storage-remote-bytes.md) 提升为正式机制）
- [0003 — Collection 是唯一组装点，无 KvRecord 宏](0003-collection-assembly-point.md)（已实现，含 save_into 跨集合原子路径）
- [0004 — Value 侧：版本化 payload、TLV 扩展区与字段 wrapper](0004-value-side-and-wrappers.md)（机制有效；宏层面被 [0006](0006-row-node-model.md) 取代——`ValueEncode` 取消，规则并入 `RowEncode` payload 半边）
- [0005 — 二级索引：item 内 slot 自动编号，索引不占手动 ns](0005-secondary-index-slots.md)（已实现；挂载点由 [0006](0006-row-node-model.md) 移到行结构体）
- [0006 — Row/Node 模型：单宏声明行，ValueEncode 并入，Node 与 Edge 平级](0006-row-node-model.md)（已实现）
- [0007 — Arrow/DataFrame 桥：行 → RecordBatch，快照分层](0007-arrow-dataframe-bridge.md)（已实现：eager 桥 + Parquet 快照；lazy pushdown 载入门控）— [中文](0007-arrow-dataframe-bridge.zh-CN.md)
- [0008 — 事件层：reduce 改名、subscribe 通道、okm-stream](0008-event-layer-reduce-subscribe-stream.md)（已实现：bare `#[kv_subscribe]`、批次 epoch、okm-stream）— [中文](0008-event-layer-reduce-subscribe-stream.zh-CN.md)
- [0009 — FRP 对照：Spacetimedb 订阅 vs okm-stream](0009-frp-comparison-subscriptions-stream.md)（分析；无实现义务）— [中文](0009-frp-comparison-subscriptions-stream.zh-CN.md)
- [0010 — VirtualStorage：KvEngine 即存储边界，远端字节走既有通道](0010-virtual-storage-remote-bytes.md)（已接受，实现待做——PLAN Phase 7）— [中文](0010-virtual-storage-remote-bytes.zh-CN.md)

实施计划见 [docs/PLAN.md](../PLAN.md)。
