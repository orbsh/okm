# ADR 目录

架构决策记录。每篇一个决策，记录真实理由（不做技术体面话包装）。

- [0001 — 边方向位 niche 进命名空间字段最高位](0001-direction-bit-niche.md)
- [0002 — 命名空间字典留在代码、永不落 KV](0002-namespace-dictionary.md)
- [0003 — Collection 是唯一组装点，无 KvRecord 宏](0003-collection-assembly-point.md)
- [0004 — Value 侧：版本化 payload、TLV 扩展区与字段 wrapper](0004-value-side-and-wrappers.md)（设计定案，实现待做）
- [0005 — 二级索引：item 内 slot 自动编号，索引不占手动 ns](0005-secondary-index-slots.md)（设计定案，实现待做；挂载点已由 [0006](0006-row-node-model.md) 移到行结构体）
- [0006 — Row/Node 模型：单宏声明行，ValueEncode 并入，Node 与 Edge 平级](0006-row-node-model.md)（设计定案，实现待做）

实施计划见 [docs/PLAN.md](../PLAN.md)。
