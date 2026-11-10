# ADR-0058：导出的表是一张 Delta 表，variant 列只有一种形态

**状态**：已接受（2026-11-10）——同轮落地：`okm_parquet::delta`（抽干 crate 的一个模块）取代 core 的 `bridge::iceberg`，开放字段列改成规范的 `struct<metadata, value>`，Iceberg 元数据编码器（含手写的 Avro 容器）被删除而不是保留。
**日期**：2026-11-10
**中文**：[0058-the-exported-table-is-a-delta-table.zh-CN.md](0058-the-exported-table-is-a-delta-table.zh-CN.md)
**依赖**：ADR-0034（表有名字，导出有表的形状）、ADR-0045（日志：捕获、段、抽干）、ADR-0056（段就是迁移通路）
**取代**：ADR-0056 §8 的 Iceberg 视图——取代的是导出目录的元数据格式，不是「目录即表」这个裁决。

## 背景

ADR-0056 §8 定下「每表一个目录」，sink 在其上写 Iceberg v2 元数据：`v1.metadata.json`、manifest list、Avro manifest、`version-hint.text`。编码器是手写的（为了让 core 保持零依赖，共 708 行，其中相当一部分是 Avro 容器格式）。关于它有两件事成立：

1. **每个窗口都重写整个视图。** snapshot append（每文件一个 snapshot）一直是延后的写入侧工作，所以读方看到的是一份列出「迄今写过的所有文件」的 snapshot，而不是一串追加。追加是 O(表)，不是 O(变化)。
2. **路径必须是绝对 `file://` URI。** 最初的设计是相对路径加 `location: "."`，理论是读方按 `location` 解析。事实不是这样：DuckDB 把 snapshot 的 `manifest-list` 按**进程工作目录**解析，于是表只在读方恰好站进表目录时才打得开。绝对 URI 修好了读，代价是搬家要重写元数据。

第三件事与格式无关：variant 列当初写成**一个扁平 `Binary`**，内容是 `[metadata][value]`，扩展名标记 `parquet.variant`。用真读方一测，这种列「任何引擎都能读成字节，没有任何引擎能解出语义」：`variant_extract(variant, 'note')` 对每一行都答 NULL，而同样的 metadata 与 value 字节放进 `struct<metadata: binary, value: binary>` 就解出了 `first`。规范的 Arrow 扩展名是 `arrow.parquet.variant`，挂在正是这个 struct 上，而 Delta 与 Iceberg v3 对 variant 列的写法都是这一种。

## 裁决

**导出目录就是一张 Delta 表。** `_delta_log/NNNNNNNNNNNNNNNNNNNN.json` 每行一个 JSON action：第一个版本声明 `protocol` 与 `metaData`，之后每个版本就是一次提交，只含窗口写出的文件对应的 `add`。表就是目录：没有 catalog、不需要注册、没有绝对路径（规范规定 `add.path` 相对表目录）。

**它住在 `okm-parquet`，不在核心。** 日志是变更日志的一次投影，比窗口再往外一层：核心产出段、合成窗口，而表**格式**拿这个窗口做什么——Parquet 文件、日志提交、回放——是抽干 crate 的活。把编码器放进核心，等于让核心拥有一张湖的布局决定权，这正是 ADR-0045 §7 与 ADR-0056「搬出去的只有格式」所禁止的。（ADR-0034 的 typed 导出路径——`to_record_batch`、图的边表、`parquet_io`——也搬了过去：core 现在完全不依赖 Arrow 与 Parquet，于是投影和被投影的类型分处两侧，由 `ToRecordBatch` / `EdgeTable` / `EdgeRestore` 三个 trait 接上。）

**variant 列声明为 `variant`**，配 `minReaderVersion 3` / `minWriterVersion 7` 与 `readerFeatures/writerFeatures = ["variantType"]`，物理形态是 `struct<metadata: binary, value: binary>` 并带 `arrow.parquet.variant` 扩展名。这个特性是表级的：不认它的读方必须拒绝整张表，而不是猜这一列是什么意思。

## 为什么是 Delta 而不是 Iceberg

- **写入器是日志，不是编码器。** 追加一个窗口是一行；没有 manifest、manifest list、snapshot、Avro 容器或版本提示，元数据从 Parquet footer 读回来，不由调用方复述。ADR-0045 延后的那件「snapshot append」在这里不是被实现，而是不再成为问题。
- **路径是相对的。** ADR-0056 §8 的搬家代价消失，连带那类曾经让该 ADR 修一轮的 bug。
- **没有东西要注册。** 读方指向目录就行。Iceberg 也能这么读，但它的元数据是一层间接（目录 → metadata.json → manifest-list → manifest），路径读方必须被告知。
- **两种格式在路径层面都不需要 catalog**——这一条是查过而不是假设的：`iceberg_scan(<dir>)` 与 `delta_scan(<dir>)` 今天都能读。促成这次讨论的「catalog」论据只对仓库引擎（Trino/Athena/BigQuery）成立，而且不是决定性论据；决定性的是写入侧。

## 放弃了什么

- **Iceberg 的 delete file。** Iceberg v2 的 positional/equality delete 是成熟件，DuckDB、Trino、Spark 今天都读；Delta 的对位是 deletion vector（Spark 3.4+，delta-rs 与 DuckDB 部分支持）。两者现在都没用上——按契约 sink 拒收删除——但等 sink 学会表达删除时，两个当中弱的那一侧现在归我们。这是开放项，不是脚注。
- **仓库广度。** Snowflake、BigQuery 原生读 Iceberg（Delta 要靠外部表 / BigLake 注册）；Athena 两边都行。对这张路是为之而建的那批读方（DuckDB、Polars、Spark）来说，什么都没变。

## 后果

- `bridge::iceberg` 删除，连带 `stamp_batch`：Delta 按**名字**认列，Parquet 的 field id 会变成没人读的第二重身份。`okm_parquet::delta` 约 350 行，没有 Avro 编码器，`parquet_io` 原样写批次。
- `Report::metadata` 数的是 Delta 提交，每表每窗口一条。
- **无符号列会显式变宽。** Delta 没有无符号整数类型，所以 `u8/u16` 变成 `short/integer`、`u32` 变成 `long`、`u64` 变成 `decimal(20,0)`。读方在 okm 的 `u64` 处看到的是 `decimal(20,0)`；替代方案要么是根本写不出的列，要么是会丢数据的窄化转换。
- **日志携带逐文件统计**：`numRecords`，以及对「Parquet 统计能表述声明类型边界」的列给出 `minValues` / `maxValues` / `nullCount`（有符号整数、浮点、截到 32 字符的字符串、布尔）。无符号列不给边界：它们的物理统计是按有符号类型算的，据此推出的边界恰好在溢出的那些行上是错的——编造边界比不做跳过更糟。
- ADR-0045 与 PLAN 里点名的 `okm-iceberg` crate 对「追加」不再需要；如果将来要做带分区、delete file/vector、文件大小目标的写入器，它仍是独立 crate——那条理由从来不是针对格式的。
- 用真读方验过，工作目录在表目录之外：DuckDB 1.5.5 `delta_scan(<dir>)` 列出各列，其中 `variant variant`；`variant_extract(variant, 'note')` 对有开放字段的那行返回 `first`，对没有的行返回 NULL。