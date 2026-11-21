# 0076 — 机制下沉到声明之下，查询面保持两套

状态：2026-11-21 裁决，同日落地。§1（唯一条目生产者）与 §2（唯一 ledger 编排）已就位；§1 现在两种形态都在跑——typed 写路径跑它发射的声明，动态 plan 跑调用方声明的 access method——§3 完成：全部机制模块搬入 core（`encode`、`path`、`access_method`、`plan_fields`、`byte_reduce`、`dynamic_collection`、`dynamic_plan`、`dict_write`、`dynamic_graph`、`dynamic_cluster`）。投影那一半去了本来就在做投影的 crate——`okm_lake::columns`；`okm-dynamic` 已删除：它的测试搬到被测代码旁，湖、日志、bindings 直接写 core 的模块路径。

## 背景

`okm-dynamic` 站在 `okm-core` 之上，却重写了一整套 core 已经拥有的机制。它有自己的条目生产者（`okm-dynamic/src/index.rs:366`，答 owned `(Vec<u8>, Vec<u8>)` 对）、自己的 ledger 编排（`okm-dynamic/src/reduce.rs:157/166` 的 `reduce_get` 与 `scan_reduces`）、自己的字段编码器、自己的 path 索引，以及它最大的两个模块——两种 schema-free 形态（`cluster.rs` 1,986 行、`graph.rs` 1,094 行，合计 3,080 行，全 crate 十二个文件 7,407 行）。同一套 entry 布局有两份实现，靠字节相等测试绑在一起，而测试只能观察到漂移，不能阻止漂移。

与此同时 core 已经拿到了动态侧「执行而不是重写」所需的东西：`CollectionSchema`/`SlotMap`/`TablePlan`（ADR-0074）、payload 解码器（ADR-0056）、带 op span 的 batch，以及一次写反复使用的 `Entries` 缓冲。`TablePlan` 还没有回答的是「一条 entry 是什么」：它今天带的是 ns 头、slot 位图与 slot 编号——那是路由——而条目仍由生成代码产出：每个 `#[ok_index]` 一个 marker 类型（`KvIndex` impl，`okm-derive/src/document_encode.rs:514`），每个 document 一个聚合体按槽位序调用它们（`:1028`）。

这一步其实已经量过也点过名了。硬编码 plan 的代价是 3.0 ns 键编码、3.0 ns 键解码、15.7 ns payload 解码，derive 是 5.2 / 3.0 / 12.1，而到 `Dict` 路径的 52.4 / 147.2 / 307.1 那段差距属于字典，不属于解释（`cargo bench -p okm-core --features fjall --bench core_paths -- plan-tax`）。代价不在解释，在两份生产者。

## 决定

1. **声明编译成两半，只有路由那一半住在 plan 上。** `TablePlan` 回答的是「一张表的字节住在哪」——键前缀、日志带哪些槽、声明给它们的编号——而它必须不会失败：日志对这张表的每个句柄只持有一份（`VirtualStorage::note_table` 收的是 `&'static TablePlan`），`TablePlan::compile` 返回 `Self` 而不是 `Result`。条目那一半是 `EntryLayout`，由 COLLECTION 持在它复用的 `Entries` 缓冲旁边：它是会被拒的那一半（`EntryLayout::compile` 返回 `Result`——面可能点名 schema 里没有的字段、把变长字段放在非末位、或者段的来源被 `func`/`where` 占了）；它是按句柄而不是按类型（动态形态在 `declare_index` 里重编它，而路由一步不动）；它只有一个读者，写路径。每个已声明的面编译一次成字段引用——键字段的静态偏移、hot 字段的偏移、cold 字段的 tag——外加它的 includes 与尾部。一次对这份布局的 walk，从键字节与写已经建好的 payload 字节把各个面追加进 collection 的 `Entries` 缓冲；reduce 的 group 段按同样方式编译。typed 侧把布局持为 `Option`，`None` 就是它字面的意思：有 `func` 面或 `where(...)` 谓词时整张表交回生成的生产者，因为一个面只能有一个生产者。生成的 `KvIndex` 实现只保留编译器能查、运行期查不了的那部分：`SLOT`、`FIELDS`、`INCLUDES`、`KEY_PREFIX`、`FUNC`、`admits`，以及 probe builder 里逐字段的 const 断言。`Document::index_entries_into` 退化成把键与 document 交给 walk 的那一次调用；每个索引自己的生成体不再生产。路由就是 derive 按表发出的那个 `static __OKM_NS_{ns}_ENTRY`，只填一次（ADR-0074 §3、ADR-0079 §2）；动态侧在 open 时编译（`TablePlan::compile`）。条目那一半由各形态自己持有的声明编译一次——typed 侧是它发射的声明表，dynamic 侧是调用方声明的 access method——因为 store 的面描述不携带它。

两种形态都在跑这套 walk，而它定下的是「一个面一个生产者」，不是「一张表一个生产者」。动态 plan 先把即将落库的 payload 编出来，再让 `Plain` 面从这些字节上走过；段不在声明字节里的面保留它自己的运行期生产者，产物追加进同一个缓冲——`Func` 的段是它 callable 的结果，`Partial` 面的准入是对 document 的谓词，`Path` 的段从动态段里解析。`DynamicCollection` 在声明到来时编译它的 `Plain` 面，之后每加一条就重编一次（`declare_index`，声明自身的规则——字段只取 payload、变长字段必须在末位——现在也在这里被拒，不再拖到第一次写）。清扫要的是 KEY 而不是键值对（`index_keys`）：删除只点名条目，而带 includes 的面那一段字节此刻没人读。`rebuild_faces` 跑的也是这套 walk，喂给它的是行槽位上存着的那个 VALUE——那个值就是这一行写入时的 payload，所以重建既不重编、也不查名字。它调 `produce` 而不是 `produce_encoded`：这些字节不是本声明的编码器产出的，walk 读到的 cold tag 值得先查一遍，查不过就不追加任何条目。两条路径一个生产者，这也是带类别的面能被重建的原因——此前的重建会报 `encode_fixed` 那句拒绝，而写路径已经不报了。面全是 `Plain` 的表因此一个 document 都不用解：去取那张 map 只是因为 callable 要从里面按名字读回字段、path 面要读动态段，所以这两次点读发不发生由声明决定，不由行决定。

有一个面是 walk 让它可以被索引、而按名字读的那套生产者拒绝过的：`Plain` 面可以点名 `Vocab` 字段。它的 payload 就是词汇表 id（ADR-0032），所以段就是那几个字节，一个类别按 id 排序——typed 侧对同一份声明本来就是这样。运行期生产者的 `encode_fixed` 没有 `Vocab` 分支，直接报错；所以这是 walk 带来的能力，是有意保留，而不是切字节的副产品。

用 bench 里那两个已声明的面——一个 hot 字段、一个 covered 字段、一个 cold `Str` 尾段——量：动态生产者 187.7 ns，walk 27.1 ns（`cargo bench -p okm-core --bench core_paths -- "dynamic-tax/(index_entries/plain|entry_layout/plain)"`；同一份代码再跑一次是 179.2 / 27.3——walk 稳定在 27 ns，生产者两次之间有几个百分点的摆动）；一次热 plan 分配 8 次，而单是键值对生产者那一段就花 17 次（`okm-core/tests/alloc_test.rs`）。差的就是字典与逐条目的 `Vec`，也就是「写手上已有的 payload 字节」这句话省下的东西。

2. **一份 ledger；错误归载体所有。** core 的 `reduce_get` / `scan_reduces` / `apply_row` 已经拥有对 accumulator 的 read-modify-write，动态侧在 `Vec<u8>` 上又写了一遍。缝本来就有：`ReduceCodec`（`encode_acc`/`decode_acc`，`okm-core/src/model/reduce.rs:26`），字节 accumulator 就是它的又一个实现。dynamic 的 fold 会失败，因为一个 `Dict` 可能对不上声明，也因为 fold 就是用户代码。给 typed 的 fold 挂上它不可能产生的错误，等于在写路径上付一笔调用方自己造不出来的账；而把 dynamic 的 fold 变成全函数，又让一个拒绝的 callable 无处可说。于是编排对载体的错误类型泛化：读侧是 `ledger_get` / `ledger_scan`，对任意 `ReduceCodec` 生效；写侧是 `apply_group`，跑在 `AccLedger` 上——载体只需回答两件事，`get_acc` 与 `put_acc`。引擎是一种载体（`StoreLedger`），dynamic 的 plan 是另一种（`PlanLedger`：一次写变成调用方记录下来的 ops）。step 拒绝时条目原样未动，因为写发生在 step 同意之后，所以形状检查不需要第二个家。`create` 在这里是参数而不是策略：typed 的写在 unfold 时也建条目，dynamic 的 plan 只在 fold 时建——这个差别比本 ADR 更老，留下来但被写明了。

3. **机制下沉，投影留在外面。** 属于机制的模块——`collection`、`plan`、`plan_fields`、`index`、`path`、`encode`、`reduce`，以及 `cluster`、`graph` 两种 schema-free 形态——移进 `okm-core`。投影不移：schema 驱动的 Arrow 列构建器（`arrow.rs`）以及它之上的一切（Parquet、湖的表格式、快照导出）留在外面，因为 core 不能依赖 arrow（ADR-0045、ADR-0056），而段的一种投影不是生产段的一部分。投影搬进 `okm-lake`（arrow 与 parquet 本来就在那里）；crate 本身被拆掉，不再充当再导出门面。

4. **查询面保持两套，这是裁决本身，不是遗留。** typed 侧按类型问：`scan::<I>(&prefix)`，marker 类型带着槽位与字段表，probe builder 把每个名字断言到键自己的表与 document 自己的表上。动态侧按编号加字节问：`scan(index_slot, encoded_prefix)`，外加 `scan_exact`、`scan_covered`、`scan_access_method_keys`，前缀由调用方编码（`encode_fields`）。两者的载体不同——编译期类型对数据——谁也不吸收谁：没有类型就构造不出 typed 探针，而宿主语言只能交出字节。统一的是**存储侧**（槽位、entry 布局、ledger），从来不是**提问侧**。

## 由此而来

- ADR-0074 §5 让两种形态**读**同一份声明；此裁决之后它们还**执行**同一个 plan。derive 与动态 codec 之间的字节相等测试从契约降为回归检查。
- `Entries`——写的缓冲，read-modify-write 时还有被替换那一行的——成为两种形态里写面生成的唯一位置。
- bindings 与 `okm-log replay` 的入口不变；它们失去的是入口背后的第二份实现。
- PLAN 里量过 walk 的那一条（「把 `TablePlan` 编译到 attach」）并入这一条，数字一并带过去。
- typed 写路径不会因为 walk 本身变快：`put` 开着 walk 与关掉 walk 量下来持平（1.2975 µs 对 1.3002 µs），因为它刚编码完的 document 本来就在 cache 里。真正把 `put` 推快的是两处：`BatchStore::put_bytes` 改成一次拷进 batch 缓冲，而不是两个 `to_vec` 加一次拷贝（1.2975 → 1.2385 µs），那是 batch 边界的修正，不是这个决定的功劳；然后是这一份 ledger，1.2385 → 1.2149 µs。条目本身那 42%（`entry_layout/compiled` 33.8 ns 对 `index_entries_into/reused` 58.4 ns）落在「字节是唯一通货」的地方，而那个地方现在接上了：动态 plan 让 `Plain` 面从刚编出来的 payload 上走过（bench 那两个面 187.7 → 27.1 ns，再跑一次 179.2 → 27.3），`index_keys` 给清扫一个只取键的形态。留在运行期生产者上的是 callable 面与 path 索引——那正是「段不再是声明字节的函数」的位置。
- 被替换那一行的面按 store 里的字节切，不按为 reduce 钩子解出来的那行切：那行经过 `__okm_deref_vocab` 后拿的是标签，而 store 里存的是 id，从它推出来的面未必指向写路径记下的那条 entry——在 vocab 字段上，merge 删不掉 put 写下的东西。
- plan 编译所用的声明不是 store 的 schema，目前也还不可能是：`IndexSchema` 带的是 `name/slot/fields/includes/func/deprecated`，所以 `key(...)` 的尾部截断与 `where(...)` 谓词在存储字节里不可见，而 reduce 声明整个不在 `CollectionSchema` 里（`faces` 只对已声明的面取指纹）。因此两个只在截断或谓词上不同的声明会描述出同一份 schema，attach 分辨不出来。补上这个洞要加宽存储的声明与它的指纹——那是一次布局代数变更（ADR-0042），是另一个决定。

## 被否

- **整包搬进 core，连 arrow 一起。** 不能向上依赖的 core 装不下 schema 驱动的 Arrow 列构建器；导出是段的一种投影，不是生产段的一部分（ADR-0045、ADR-0056 §8）。
- **把查询面统一到运行期、按名字索引的探针上。** 它把字段名与宽度错误从编译期推到运行期，推的正是有类型的那一侧；对动态侧则什么都没给——它今天就是交编码字节。
- **保留两份生产者、靠字节相等测试兜底。** 测试能观察到漂移，阻止不了漂移；而且漂移在读侧是静默的——一条排序位置变了的 entry 仍然是一条格式良好的 entry。
- **按 collection 编译或缓存 plan。** typed 的 plan 是 const；缓存等于把 plan 存在的意义要消掉的那次查表，重新装回写路径。
- **删掉 `okm-dynamic`、用它的名字 re-export core。** 名字会比 crate 活得久，却指向另一个实体；投影那半仍然需要落点，而 re-export 给不了落点。一个整库都是 `pub use` 的 crate 换来的是名字稳定与一份裁剪过的名字清单，换不来边界：`pub use` 是透明的，消费方随时能写出真实路径，而每次机制搬家都要在这里手工镜像。这笔账在「表面要被工作区外的人版本化」时才划算；这里 bindings、湖、日志与 core 在同一个 commit 上编译，所以这层唯一保证的事情是「搬一次家要改两处」。
