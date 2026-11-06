# ADR-0055：函数索引的结果是一个迭代器

日期：2026-11-06
状态：已接受（已落地：删除 `IndexFuncValues`、新增 `result_segments`、两处生成代码切换、所有单值声明改 `[String; 1]`、数组/迭代器/空集三种形态有测试、`func_returns_a_plain_value` 编译失败测试）

## 背景

ADR-0005 引入 `func(path)` 时写的是「返回值类型必须实现 `IndexFuncResult`」——一个值，一条 entry。ADR-0015 顺手提过函数返回 `Vec<V>` 会 fan out。两篇都没有记录它们之间长出来的那个决定：机制同时接受两种形态（一个值，或它的一串），靠一对 trait 承担——`IndexFuncResult` 管编码，`IndexFuncValues` 管形态——而单值那臂是五个类型逐个写出来的 impl，因为 blanket 的 `impl<T: IndexFuncResult> IndexFuncValues for T` 会与 `Vec<V>` 臂重叠，Rust 拒绝重叠实现。

这层 adapter 不是白来的：多一个要学的 trait；一个只干「列出五个类型」的宏；一条让读者第一次见到某个 trait 的错误信息；以及每次解释这个特性都要附一句 coherence 注脚。而它适配的两种形态本来是同一种形态。函数索引要的不是「一个值或一个 Vec」，是「这一行贡献哪些值」——Rust 已经有这个东西：`IntoIterator`。

## 决策

**1. 只有一个契约：声明的函数返回某个可迭代出 `IndexFuncResult` 的东西。**

```text
fn lower_name(row: &Doc) -> [String; 1] { [row.name.to_lowercase()] }   // 一条 entry
fn tokens(row: &Doc)     -> Vec<String> { …split_whitespace… }          // N 条
fn buckets(row: &Row)    -> impl Iterator<Item = u64> { … }             // N 条，惰性
```

单值写成单元素数组——不分配，且读出来就是「恰好一个」。空迭代器仍然是零条 entry，也是函数体里逐值过滤的写法。引用与值的段编码相同（`impl<T: IndexFuncResult + ?Sized> IndexFuncResult for &T`），所以函数可以直接迭代行里已有的集合（`row.ids.elems.iter()`），不必先 clone 成临时集合再让写入端走一遍。

**2. `IndexFuncValues` 删除，由 `result_segments` 取代。** `okm-core` 里的自由函数，泛型于 `I: IntoIterator<Item = V>, V: IndexFuncResult`，每个值产出一个段缓冲区。它是泛型函数而非 trait：除了 `IntoIterator` 本身，没有别的分派，所以不存在「两臂互相 overlap」这回事。

**3. 两处生成代码不再区分形态。** `entry_pairs` 遍历 `result_segments(#fpath(document))`，每个元素建一条 entry，外层容量取自 `size_hint` 而不是机制已经没有的 `Vec::len()`；`encode_named` 取 `.next()`——第一条段，对单值结果就是全部——不再用 `.pop()`，那个「最后一条」只是随手取一个的副作用。

**4. `IndexFuncResult` 不变**：`String` → 裸 UTF-8 字节，`u8`/`u16`/`u32`/`u64` → BE。五个之外的值类型只需实现它，不需要别的。

## 备选

- **只留 Vec 一种形态**——函数返回 `Vec<V>`，单值就是单元素 `Vec`。统一，同样能删掉那个 trait。否决：每个单值点位每次 put 多一次 `vec![x]` 分配，签名多一层 `vec![…]` 包装，而探针侧会多出一个机制答不了的问题（哪一个元素才是探针？）。一行一个值是常见情形，为了简化少见情形把噪声和分配压到常见情形上，方向反了。
- **保留两臂**。能用；代价是解释。否决的理由是「放弃了一次简化」，不是缺陷。
- **`impl IndexFuncResult for Vec<T>`**——元素之间必须加帧才能区分，那就按长度先排，字典序被毁掉。

## 后果

- 破坏性变更，主动接受（无生产数据）：返回 `String` 的 `func` 现在编译失败，报 `` `String` is not an iterator … required by a bound in `result_segments` ``，改法是 `[String; 1]`。由 `tests/compilefail/func_returns_a_plain_value.rs` 锁住。
- adapter、它的宏、它的 coherence 注脚全部消失。值类型实现一个 trait；多值结果什么都不用实现——可迭代就行。
- 写路径少一层中间 `Vec<Vec<u8>>`：段边产边消费，不再先收集再用 `len()` 量。
- 探针语义不变（查询侧对探针值调用同一路径、取第 0 个元素），但机制不再假装多值结果有「正统探针」。值本身无序那是建模问题，写入端解决不了。
- `encode_named` 的「第一条元素就是段」从副作用变成明文规定。
- 动态（bindings）路径不动：`AccessMethodKind::Func` 收的是返回**已编码段**（`Vec<Vec<u8>>`）的闭包——穿越 FFI 的 boxed closure 没有元素类型可供泛型。它本来就是一种形态（字节），也保持这一种；两条路径的差别在「编码发生在哪」，不在「接受几种形态」。

## 参考

ADR-0005（函数索引）、ADR-0015 §4（`Vec` fan-out 的提法）、`okm-core/src/model/index.rs`、`okm-derive/src/document_encode.rs`、`docs/internals/func-index-mechanism.zh-CN.md`。