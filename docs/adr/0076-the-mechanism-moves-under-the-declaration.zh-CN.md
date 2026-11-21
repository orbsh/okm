# 0076 — 机制下沉到声明之下，查询面保持两套

状态：2026-11-21 裁决。§1、§2 已落地；§3 进行中——`encode`、`path`、`plan_fields` 与声明式索引那一套（`index` → `model::access_method`，与 core 已有的 typed 半边并排）`reduce`（字节 fold 是 `model::byte_reduce::ByteReduce`）以及 dynamic 门面本身——`collection`、`plan`、`write`——已搬入 core；留在外面的是 `arrow` 与各个投影。

## 背景

`okm-dynamic` 站在 `okm-core` 之上，却重写了一整套 core 已经拥有的机制。它有自己的条目生产者（`okm-dynamic/src/index.rs:366`，答 owned `(Vec<u8>, Vec<u8>)` 对）、自己的 ledger 编排（`okm-dynamic/src/reduce.rs:157/166` 的 `reduce_get` 与 `scan_reduces`）、自己的字段编码器、自己的 path 索引，以及它最大的两个模块——两种 schema-free 形态（`cluster.rs` 1,986 行、`graph.rs` 1,094 行，合计 3,080 行，全 crate 十二个文件 7,407 行）。同一套 entry 布局有两份实现，靠字节相等测试绑在一起，而测试只能观察到漂移，不能阻止漂移。

与此同时 core 已经拿到了动态侧「执行而不是重写」所需的东西：`CollectionSchema`/`SlotMap`/`TablePlan`（ADR-0074）、payload 解码器（ADR-0056）、带 op span 的 batch，以及一次写反复使用的 `Entries` 缓冲。`TablePlan` 还没有回答的是「一条 entry 是什么」：它今天带的是 ns 头、slot 位图与 slot 编号——那是路由——而条目仍由生成代码产出：每个 `#[ok_index]` 一个 marker 类型（`KvIndex` impl，`okm-derive/src/document_encode.rs:514`），每个 document 一个聚合体按槽位序调用它们（`:1028`）。

这一步其实已经量过也点过名了。硬编码 plan 的代价是 3.0 ns 键编码、3.0 ns 键解码、15.7 ns payload 解码，derive 是 5.2 / 3.0 / 12.1，而到 `Dict` 路径的 52.4 / 147.2 / 307.1 那段差距属于字典，不属于解释（`cargo bench -p okm-core --features fjall --bench core_paths -- plan-tax`）。代价不在解释，在两份生产者。

## 决定

1. **plan 是唯一的条目生产者。** `TablePlan` 长出一个 `EntryLayout`：每个已声明的面编译一次成字段引用——键字段的静态偏移、hot 字段的偏移、cold 字段的 tag——外加它的 includes 与尾部。一次对这份布局的 walk，从键字节与写已经建好的 payload 字节把各个面追加进 collection 的 `Entries` 缓冲；reduce 的 group 段按同样方式编译。生成的 `KvIndex` 实现只保留编译器能查、运行期查不了的那部分：`SLOT`、`FIELDS`、`INCLUDES`、`KEY_PREFIX`、`FUNC`、`admits`，以及 probe builder 里逐字段的 const 断言。`Document::index_entries_into` 退化成把键与 document 交给 walk 的那一次调用；每个索引自己的生成体不再生产。路由在 typed 侧仍是 const（`Document::PLAN`，ADR-0074 §3），在 dynamic 侧于 open 时编译（`TablePlan::compile`）；条目那一半由各形态自己持有的声明编译一次——typed 侧是它发射的声明表，dynamic 侧是调用方声明的 access method——因为 store 的面描述不携带它。一个生产者，两种拿到声明的方式。

2. **一份 ledger；错误归载体所有。** core 的 `reduce_get` / `scan_reduces` / `apply_row` 已经拥有对 accumulator 的 read-modify-write，动态侧在 `Vec<u8>` 上又写了一遍。缝本来就有：`ReduceCodec`（`encode_acc`/`decode_acc`，`okm-core/src/model/reduce.rs:26`），字节 accumulator 就是它的又一个实现。dynamic 的 fold 会失败，因为一个 `Dict` 可能对不上声明，也因为 fold 就是用户代码。给 typed 的 fold 挂上它不可能产生的错误，等于在写路径上付一笔调用方自己造不出来的账；而把 dynamic 的 fold 变成全函数，又让一个拒绝的 callable 无处可说。于是编排对载体的错误类型泛化：读侧是 `ledger_get` / `ledger_scan`，对任意 `ReduceCodec` 生效；写侧是 `apply_group`，跑在 `AccLedger` 上——载体只需回答两件事，`get_acc` 与 `put_acc`。引擎是一种载体（`StoreLedger`），dynamic 的 plan 是另一种（`PlanLedger`：一次写变成调用方记录下来的 ops）。step 拒绝时条目原样未动，因为写发生在 step 同意之后，所以形状检查不需要第二个家。`create` 在这里是参数而不是策略：typed 的写在 unfold 时也建条目，dynamic 的 plan 只在 fold 时建——这个差别比本 ADR 更老，留下来但被写明了。

3. **机制下沉，投影留在外面。** 属于机制的模块——`collection`、`plan`、`plan_fields`、`index`、`path`、`encode`、`reduce`，以及 `cluster`、`graph` 两种 schema-free 形态——移进 `okm-core`。投影不移：schema 驱动的 Arrow 列构建器（`arrow.rs`）以及它之上的一切（Parquet、湖的表格式、快照导出）留在外面，因为 core 不能依赖 arrow（ADR-0045、ADR-0056），而段的一种投影不是生产段的一部分。`okm-dynamic` 保留投影，以及湖、日志、bindings 读的那些 re-export；它的机制模块是移走，不是再 re-export 一遍。

4. **查询面保持两套，这是裁决本身，不是遗留。** typed 侧按类型问：`scan::<I>(&prefix)`，marker 类型带着槽位与字段表，probe builder 把每个名字断言到键自己的表与 document 自己的表上。动态侧按编号加字节问：`scan(index_slot, encoded_prefix)`，外加 `scan_exact`、`scan_covered`、`scan_access_method_keys`，前缀由调用方编码（`encode_fields`）。两者的载体不同——编译期类型对数据——谁也不吸收谁：没有类型就构造不出 typed 探针，而宿主语言只能交出字节。统一的是**存储侧**（槽位、entry 布局、ledger），从来不是**提问侧**。

## 由此而来

- ADR-0074 §5 让两种形态**读**同一份声明；此裁决之后它们还**执行**同一个 plan。derive 与动态 codec 之间的字节相等测试从契约降为回归检查。
- `Entries`——写的缓冲，read-modify-write 时还有被替换那一行的——成为两种形态里写面生成的唯一位置。
- bindings 与 `okm-log replay` 的入口不变；它们失去的是入口背后的第二份实现。
- PLAN 里量过 walk 的那一条（「把 `TablePlan` 编译到 attach」）并入这一条，数字一并带过去。
- typed 写路径不会因为 walk 本身变快：`put` 开着 walk 与关掉 walk 量下来持平（1.2975 µs 对 1.3002 µs），因为它刚编码完的 document 本来就在 cache 里。真正把 `put` 推快的是两处：`BatchStore::put_bytes` 改成一次拷进 batch 缓冲，而不是两个 `to_vec` 加一次拷贝（1.2975 → 1.2385 µs），那是 batch 边界的修正，不是这个决定的功劳；然后是这一份 ledger，1.2385 → 1.2149 µs。条目本身那 42%（`entry_layout/compiled` 33.8 ns 对 `index_entries_into/reused` 58.4 ns）落在「字节是唯一通货」的地方：dynamic 形态，以及任何 payload 到手、身边没有 typed document 的生产者。
- 被替换那一行的面按 store 里的字节切，不按为 reduce 钩子解出来的那行切：那行经过 `__okm_deref_vocab` 后拿的是标签，而 store 里存的是 id，从它推出来的面未必指向写路径记下的那条 entry——在 vocab 字段上，merge 删不掉 put 写下的东西。
- plan 编译所用的声明不是 store 的 schema，目前也还不可能是：`IndexSchema` 带的是 `name/slot/fields/includes/func/deprecated`，所以 `key(...)` 的尾部截断与 `where(...)` 谓词在存储字节里不可见，而 reduce 声明整个不在 `CollectionSchema` 里（`faces` 只对已声明的面取指纹）。因此两个只在截断或谓词上不同的声明会描述出同一份 schema，attach 分辨不出来。补上这个洞要加宽存储的声明与它的指纹——那是一次布局代数变更（ADR-0042），是另一个决定。

## 被否

- **整包搬进 core，连 arrow 一起。** 不能向上依赖的 core 装不下 schema 驱动的 Arrow 列构建器；导出是段的一种投影，不是生产段的一部分（ADR-0045、ADR-0056 §8）。
- **把查询面统一到运行期、按名字索引的探针上。** 它把字段名与宽度错误从编译期推到运行期，推的正是有类型的那一侧；对动态侧则什么都没给——它今天就是交编码字节。
- **保留两份生产者、靠字节相等测试兜底。** 测试能观察到漂移，阻止不了漂移；而且漂移在读侧是静默的——一条排序位置变了的 entry 仍然是一条格式良好的 entry。
- **按 collection 编译或缓存 plan。** typed 的 plan 是 const；缓存等于把 plan 存在的意义要消掉的那次查表，重新装回写路径。
- **删掉 `okm-dynamic`、用它的名字 re-export core。** 名字会比 crate 活得久，却指向另一个实体；投影那半仍然需要落点，而 re-export 给不了落点。