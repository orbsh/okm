# 0076 — 机制下沉到声明之下，查询面保持两套

状态：2026-11-21 裁决，尚未落地。

## 背景

`okm-dynamic` 站在 `okm-core` 之上，却重写了一整套 core 已经拥有的机制。它有自己的条目生产者（`okm-dynamic/src/index.rs:366`，答 owned `(Vec<u8>, Vec<u8>)` 对）、自己的 ledger 编排（`okm-dynamic/src/reduce.rs:157/166` 的 `reduce_get` 与 `scan_reduces`）、自己的字段编码器、自己的 path 索引，以及它最大的两个模块——两种 schema-free 形态（`cluster.rs` 1,986 行、`graph.rs` 1,094 行，合计 3,080 行，全 crate 十二个文件 7,407 行）。同一套 entry 布局有两份实现，靠字节相等测试绑在一起，而测试只能观察到漂移，不能阻止漂移。

与此同时 core 已经拿到了动态侧「执行而不是重写」所需的东西：`CollectionSchema`/`SlotMap`/`TablePlan`（ADR-0074）、payload 解码器（ADR-0056）、带 op span 的 batch，以及一次写反复使用的 `Entries` 缓冲。`TablePlan` 还没有回答的是「一条 entry 是什么」：它今天带的是 ns 头、slot 位图与 slot 编号——那是路由——而条目仍由生成代码产出：每个 `#[ok_index]` 一个 marker 类型（`KvIndex` impl，`okm-derive/src/document_encode.rs:514`），每个 document 一个聚合体按槽位序调用它们（`:1028`）。

这一步其实已经量过也点过名了。硬编码 plan 的代价是 3.0 ns 键编码、3.0 ns 键解码、15.7 ns payload 解码，derive 是 5.2 / 3.0 / 12.1，而到 `Dict` 路径的 52.4 / 147.2 / 307.1 那段差距属于字典，不属于解释（`cargo bench -p okm-core --features fjall --bench core_paths -- plan-tax`）。代价不在解释，在两份生产者。

## 决定

1. **plan 是唯一的条目生产者。** `TablePlan` 长出产一条 entry 所需的布局——每个已声明槽位的索引字段位置与宽度、includes、reduce 的 group 段——再由一次对它的 walk 把各个面追加进 collection 的 `Entries` 缓冲。生成的 `KvIndex` 实现只保留编译器能查、运行期查不了的那部分：`SLOT`、`FIELDS`、`INCLUDES`、`KEY_PREFIX`、`FUNC`、`admits`，以及 probe builder 里逐字段的 const 断言。`Document::index_entries_into` 退化成把键与 document 交给 walk 的那一次调用；每个索引自己的生成体不再生产。typed 的 plan 是一个 const（`Document::PLAN`，ADR-0074 §3），dynamic 的 plan 是同一个值在 open 时编译出来（`TablePlan::compile`）——一个生产者，两种到手方式。

2. **一份 ledger，且 fold 保持不失败。** core 的 `reduce_get` / `scan_reduces` / `apply_row` 已经拥有对 accumulator 的 read-modify-write，动态侧在 `Vec<u8>` 上又写了一遍。缝本来就有：`ReduceCodec`（`encode_acc`/`decode_acc`，`okm-core/src/model/reduce.rs:26`），字节 accumulator 就是它的又一个实现。dynamic 的 fold 会失败，因为一个 `Dict` 可能对不上声明；但这个检查已有边界：动态层在规划任何东西之前就拒绝这种 document（`index_entries` 与 `plan.rs` 都答 `Result<_, String>`），所以形状检查留在那条边界上，共享的 fold 步保持不失败。给 typed 的 fold 挂 `Result`，等于在写路径上为一个不可能发生、且调用方无法处理的错误付账。

3. **机制下沉，投影留在外面。** 属于机制的模块——`collection`、`plan`、`plan_fields`、`index`、`path`、`encode`、`reduce`，以及 `cluster`、`graph` 两种 schema-free 形态——移进 `okm-core`。投影不移：schema 驱动的 Arrow 列构建器（`arrow.rs`）以及它之上的一切（Parquet、湖的表格式、快照导出）留在外面，因为 core 不能依赖 arrow（ADR-0045、ADR-0056），而段的一种投影不是生产段的一部分。`okm-dynamic` 保留投影，以及湖、日志、bindings 读的那些 re-export；它的机制模块是移走，不是再 re-export 一遍。

4. **查询面保持两套，这是裁决本身，不是遗留。** typed 侧按类型问：`scan::<I>(&prefix)`，marker 类型带着槽位与字段表，probe builder 把每个名字断言到键自己的表与 document 自己的表上。动态侧按编号加字节问：`scan(index_slot, encoded_prefix)`，外加 `scan_exact`、`scan_covered`、`scan_access_method_keys`，前缀由调用方编码（`encode_fields`）。两者的载体不同——编译期类型对数据——谁也不吸收谁：没有类型就构造不出 typed 探针，而宿主语言只能交出字节。统一的是**存储侧**（槽位、entry 布局、ledger），从来不是**提问侧**。

## 由此而来

- ADR-0074 §5 让两种形态**读**同一份声明；此裁决之后它们还**执行**同一个 plan。derive 与动态 codec 之间的字节相等测试从契约降为回归检查。
- `Entries`——写的缓冲，read-modify-write 时还有被替换那一行的——成为两种形态里写面生成的唯一位置。
- bindings 与 `okm-log replay` 的入口不变；它们失去的是入口背后的第二份实现。
- PLAN 里量过 walk 的那一条（「把 `TablePlan` 编译到 attach」）并入这一条，数字一并带过去。

## 被否

- **整包搬进 core，连 arrow 一起。** 不能向上依赖的 core 装不下 schema 驱动的 Arrow 列构建器；导出是段的一种投影，不是生产段的一部分（ADR-0045、ADR-0056 §8）。
- **把查询面统一到运行期、按名字索引的探针上。** 它把字段名与宽度错误从编译期推到运行期，推的正是有类型的那一侧；对动态侧则什么都没给——它今天就是交编码字节。
- **保留两份生产者、靠字节相等测试兜底。** 测试能观察到漂移，阻止不了漂移；而且漂移在读侧是静默的——一条排序位置变了的 entry 仍然是一条格式良好的 entry。
- **按 collection 编译或缓存 plan。** typed 的 plan 是 const；缓存等于把 plan 存在的意义要消掉的那次查表，重新装回写路径。
- **删掉 `okm-dynamic`、用它的名字 re-export core。** 名字会比 crate 活得久，却指向另一个实体；投影那半仍然需要落点，而 re-export 给不了落点。