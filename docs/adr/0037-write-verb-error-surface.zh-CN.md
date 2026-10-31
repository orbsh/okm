# ADR-0037：写动词返回 `Result`

状态：Accepted（2026-10-24）。对 ADR-0035 的一处细化；初稿之后拓宽到每一个写动词——只改面向 map 的那半会让 typed 动词继续 panic，而一个报不出拒写的动词等于没有错误通道。

## 背景

`merge` 与 `replace` 接收调用方构造的 map——HTTP body、解出来的 Parquet 行、宿主语言的 dict。此前，名字命中声明字段但 `DynamicValue` 种类不对时，会被**静默**替换成该字段的声明默认值。ADR-0035 已把这件事记为缺陷（"在这里顺手拒绝名字命中声明字段但值类型不符"）并列为独立改动；第一版修复把它改成了 panic。

panic 是错误的通道，因为本库只有两类失败，而它们的区别在于**谁能处理**：

- **调用方数据**——map 不符合声明。这是正常输入（一个 400，不是故障），只有调用方能修。
- **引擎**——存储拒绝了这次提交（写者被 fence、I/O 失败、远端帧错误）。与调用方无关，重要的是引擎自己的原文。

只有第一类在设计上是可恢复的，而它恰好出现在调用方数据进入的地方：面向 map 的动词。

## 决定

1. **`okm_core::Error`**（`thiserror`）是本 crate 的错误类型，落在
   `okm-core/src/error.rs`，并配 `pub type Result<T> = std::result::Result<T, Error>`：
   - `KindMismatch { field, want, got }`——名字在，种类不对。
   - `ValueOutOfRange { field, want, got }`——种类对，值放不进声明宽度。
   - `VocabularyRequired { name }`——嵌套对象到了没有词表的编码器（原先是 panic）。
   - `VocabularyExhausted { width, next }`——某字段的 id 宽度用尽（原先是 panic）。
   - `EdgeAlreadyLive { edge_id }`、`EdgeNotFound { edge_id }`、
     `EdgeNotMember { side, ns, graph_ns }`——图自己的拒绝，原先用 `String`。
   - `Engine(String)`——引擎拒绝，内嵌它自己的原文。
2. **`Document::from_map` 改为 `try_from_map -> Result<Self>`。** 判据是**有没有这个名字**：缺席的名字取声明默认（append-only 演进规则，与 payload decoder 同一条），出现的名字必须符合声明。
3. **每个写动词都可失败**——`put`、`upsert_with`、`purge`、`prune_deprecated_slots`、`merge`、`replace`、`save_with_prev_into`、junction 的 `link` / `unlink`，以及 graph 的 `link` / `link_into` / `unlink` / `unlink_into` / `replace_into`。没有写动词会 panic：引擎拒绝是 `Error::Engine`，词表满 `VocabularyExhausted`，调用方的 map 不合声明 `KindMismatch` / `ValueOutOfRange`，边的错 `EdgeAlreadyLive` / `EdgeNotFound` / `EdgeNotMember`。intern 钩子整条链可失败（`Document::__okm_intern`、`VocabCache::id_for`、`put_frame*`）——intern 是一次写里唯一的分配动作，而它能失败。map 的校验发生在任何 op 进入 batch 之前，所以被拒绝的 `merge` / `replace` **什么都没写**——动态半边也一样。

## 否决的备选

- **到处 panic**（此前状态，也是第一版修复）。调用方无法处理"调用方数据"这类失败：一个烂请求体最好的结果是断连，最差是进程死掉——而且类型上分不清"你的数据"和"磁盘"。
- **用 `String` 报错**，即 `okm-dynamic` 的形态。失去可匹配性：想回一个带字段名的 400，就得解析文本。动态侧保留自己的类型，typed 侧拿到真正的 enum。
- **半 Result：`merge` 可失败、底下的 `try_from_map` 仍 panic。** 动词返回 `Result` 而它的构造器会 panic，这个动词就无法被安全使用——失败逃出类型系统，恰恰是本 ADR 要终结的状态。
- **只改面向 map 的动词。** 第一版把界线画在"map 是调用方数据"上，typed 动词因为入参已是值而继续 panic。这话读着顺，直到第一次引擎拒绝：只用 `put` 的服务在那里死掉，而同一个存储错误走 `merge` 只是一个返回值。问题从来不在于入参——在于**错误没有出口**；单写者契约是并发论证，不是错误处理论证。所有写动词一条规则，值得那趟搬家（仓外约 87 处，机械补 `?`）。

## 后果

- 被拒绝的 `merge` / `replace` 不改动存储，并报出字段名、声明种类、实际种类。
  `a_refused_merge_returns_the_error_and_writes_nothing` 锁住"什么都没写"这一半。
- 被拒绝的写不动存储：校验与 intern 都在任何 op 进 batch 之前跑，`commit` 是每个动词的最后一句。
- 没有写动词会 panic。仍然 panic 的是**读**侧对损坏字节（`decode_payload`、edge body 解码）与逐条底层存储方法（`VirtualStorage::put` / `del`）——两者都是"存储不是它声称的样子"，不是调用方能处理的条件。拓宽任何一边都是另一个题目（完整性报告、存储 trait）。
- 还剩一处写路径 panic，在**编码面**而不是动词里：`Document::encode_payload` 不 intern——
  它收一个文档、回一串字节——所以有东西直接调编码器时，一个手工构造、`Vocab` 从未 intern
  过的文档会死在 `Vocab::wire16`。没有动词会走到那里：`put`、`save_with_prev_into` 与
  `purge_into` 都先 intern，而曾经压在这条接缝上的那个只管编码的批动词已经移除（ADR-0031）。
  把 payload 编码做成可失败意味着改 `encode_payload`——每个索引 entry、批写者和导出路径都
  调用的 trait 方法——那是另一个改动。
- 下游迁移是机械的：调用方本来就返回 Result 的地方补 `?`，测试里补 `.unwrap()`。仓外每个写调用点（约 87 处）都要动，所以这次作为一次 API 断裂发布，而不是留一长串半改的动词。