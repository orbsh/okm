# ADR-0038：读失败不是行不存在

状态：已接受（2026-10-24）。与 ADR-0037 配对——那一篇给写动词错误通道，这一篇给读动词。

## 背景

所有读动词的返回类型里只容得下一种坏消息：`Option`。缺席与故障是同一个答案，而且是从最底层一路吞上来的：

- `WireClient::get` 是 `.ok().and_then(|r| r.value)`，`scan_suffix` 是 `.ok()`；
- fjall 后端的 `scan_suffix` 是 `filter_map(|k| k.ok())`——中途失败被丢掉，返回一个更短的答案；
- fjall / slatedb 的 `get` 是 `expect(...)`，即库内部 panic；
- wire 应答根本没有错误字段，接收侧就算知道，也传不回来。

后果是 KV 存储最不能产生的那一种：先读后写的消费方（prism 与 aura 的 `store-exec` 正是这个形状）读到 `None`，判定"这行不存在"，然后覆盖。一次网络抖动变成一次静默的数据丢失。

## 决定

1. **缺席继续用 `Option`，故障用 `Result`。** 它们是两个不同的答案，各自保留类型：语义读动词返回 `Result<Option<R>>`（`get`、`fetch`、`edge`、`get_edge`、`attrs`、`reduce_get`、`predicate_vocab_name`），集合形状的返回 `Result<Vec<…>>`（`scan`、`scan_exact`、`scan_range`、`scan_covered`、`scan_keys`、`scan_document_keys`、`scan_documents_raw`、`slot1_map`、`edge_ids`、`predicates`、junction 的 `forward` / `reverse*` 全家、图的遍历全家）。把两者合成一个三态枚举被否决：那让每个调用点为一个多数调用方不需要的区分付费。
2. **存储边界说 `Result`。** `VirtualStorage` / `VirtualStorageAsync` 的 `get`、`scan_range`、`scan_suffix`、`commit_batch` 全部返回 `crate::error::Result`，`ScanIter::Item` 变成 `Result<(Vec<u8>, Vec<u8>)>`——中途死掉的遍历必须在死掉的那一点说出来，而不是提前结束。六个后端（fjall、redb、slatedb、localstorage、测试引擎、remote）跟上，没有一个还在读路径上 `expect`。redb 因为每种操作抛不同类型，新增 `fn eng<E: Display>(e: E) -> Error`。
3. **wire 应答可以是一个 fault。** `OpResponse` 新增 `fault: Option<String>`，编码成独立的 `[2][len][utf8]` 帧。`0`（缺席）与 `1`（有值）逐字节不变，所以读旧文法的一方行为不变；`2` 原本是畸形帧、解码成 `None`，没有消费方能把它误读成数据。fault 应答不带 value、不带 suffixes、不带 page——一个失败的答案本来就没有这些东西。接收侧负责设它（`OP_GET`、`OP_SCAN`、以及 `OP_SCAN_STREAM` 的每一页——那一页失败就中断遍历，而不是静默截断）；发送侧把它折回 `Err`。
4. **三个读钩子变成可失败**，因为它们要读：`Document::__okm_embed_deref`、`Document::__okm_deref_vocab`，以及那个藏在写路径后面的 `Document::__okm_apply_reduces`——它在折叠之前先读组的累加器。junction 生成的两个查询方法（`get_session`、`get_user`）跟上：它们调 `forward` / `reverse_raw`。动态侧也跟上，而它的形状正好说明规则不针对某个动词：`AccOf`——调用方提供的「这个组当前累加器是多少」闭包——原本返回 `Option<Vec<u8>>`，于是每个实现都写成 `&|ek| store.get(ek).unwrap()`，存储故障在 plan 调用的闭包里 panic。它现在返回 `Result<Option<Vec<u8>>, String>`，`okm_dynamic::reduce_get` 一同改。
5. **两处存储违约不再 abort。** junction 的 `forward` 在端点集合被截断时无法把对端解码回 key 类型（`Error::Corrupt`）；`BatchStore` 根本不能被扫描（`Error::ScanThroughBatch`）。两者都是用错调用方，按错误报告。
6. `Error::Engine` 不再渲染成 "engine commit failed"：读也会走到这个变体。

## 明确不做

`decode_payload` 在字节解不开时仍然 panic。损坏字节意味着存储返回了它承诺不会返回的东西，调用方对它没有可执行动作；要把它做成 `Err`，得把 `Result` 穿过每个 derive 解码器、每个索引解码、覆盖段解码和 Parquet 导出路径——那比整场重构还大，为的是一个没人能处理的错误。它保留为大声的中止，并在它所在的地方写明——ADR-0040 给出了它所属的通用规则：panic 错在「调用方能对失败做事」的场合，而不是到处都错。

## 否决的替代方案

- **`Option` 加旁路通道**（thread-local 记录最后一个错误、存储上的标记位）。类型层面不可见：真正需要这个区分的调用点，正是会忘记去问的那个。
- **把故障折进缺席**（即原来的状态）。这就是缺陷本身。
- **`decode_payload` 也返回 `Result<Option<T>>`。** 见上：有成本，没消费方。
- **fault 应答仍带 `suffixes` / `hits`。** 那会让一页半失败的结果看起来是完整的一页；失败不是一页。

## 后果

- 每个读调用点自己说出想要什么：在乎失败的地方用 `?`，测试与 bench 里存储已知可用处用 `.unwrap()`。
- wire 编解码器第一次有了属于自己的测试（往返、缺席、两种文法下的 fault、fault 帧拒绝尾随字节）。
- 同一次排查还带出一处清理：如果调用方交进来的闭包自己会 panic，外层的 `Result` 一点用都没有——这正是 `AccOf` 原来的形状。
- 同一次排查还带出一处清理：调用方交进来的闭包自己会 panic 的话，外层的 `Result` 一点用都没有——这正是 `AccOf` 原来的形状。
- 落地途中 `kv_nest_test::nest_derive_end_to_end` 抓到一个真实回归：接收侧**不能**对前缀扫描的答案剥托管前缀（那些后缀本来就是相对托管 key 的），该剥的是区间扫描的答案——那才是 wire 契约要求的那一半。