# ADR-0023: 预置 reduce 组合子——`Count`、`Max`、`Min`、`Sum` 作为库声明

> **Languages:** [English](0023-preset-reduce-combinators.md)（主文档） · [中文](0023-preset-reduce-combinators.zh-CN.md)

**状态**：已接受（2026-09-22）；2026-09-23 落地，附修订如下

> **修订（2026-09-23，实现时）。**（1）最终集合为 `Count`、`Sum`、`HighWater`、`LowWater`——裸 `Max<F>`/`Min<F>` 刻意不提供：`u64` 累加器内真正可逆的 un-extreme unfold 需要第二结构恢复前一个极值，这正是预置组合子要消灭的仪式；HighWater/LowWater 名字即真实语义（两者 unfold 均为 no-op——watermark 契约）。（2）不分组模式：省略 group 块（`#[ok_reduce(Count)]`）即声明全表单一组，entry key 为 `[ns 2B][slot 2B]`、无 group 段。（3）`LowWater` 的累加器是 `LowAcc`（newtype，`Default` 即单位元 `u64::MAX`）：typed 读-改-写以 `Default::default()` 播种 acc，单位元必须住在类型里，而非 fold 时的特判。（4）**Sum 的累加器类型跟字段走**：无符号 payload 字段按 u64 累加，有符号 payload 字段（i8–i64）按 i64，`Quant<f64, P>` 在其精确的定点 i64 wire 域内累加——绝不用裸浮点（次序无关性）。这要求 derive 支持有符号整数 payload 编码（wire 与无符号同宽同 BE，two's complement 即 BE 编码——不新增 FieldType kind）。watermark 组合子保持无符号域：`KeyEncode` 的 wire 不携带符号，且 watermark 契约本就是无符号域契约。

## 背景

reduce 是 okm 的跨文档预计算面（ADR-0008）：用户实现 `ReduceLogic`（Acc + fold/unfold），由 `#[ok_reduce(Logic { group(..) })]` 声明驱动，fold 钩子活在写路径的引擎锁内读-改-写里。机制刻意做成声明式——语义用户给、机制框架给——写路径成本只由声明了 reduce 的表支付。

契约成立，但每个真实使用都以同样的方式开始：用户手写一个 `ReduceLogic` impl，其 fold/unfold 无非是几种标准形状之一——数行数、维护最大值、对一个字段求和。第一个生产消费者（aura 的 state 表，其 id 分配骑在一个 MAX reduce 上）给出四点观察：

1. **样板占了大头。** 一个 `MaxInstanceId` 需要：一个 unit struct、一个 `impl ReduceLogic`（Document/Acc/fold/unfold）、一个 payload 镜像字段让 reduce 钩子看到被聚合的值、以及通过 `reduce_get` 读 watermark。聚合语义是两个字符（`max`）；仪式是十五行。
2. **常见形状本就受限。** 对 payload 字段做 Count/Max/Min/Sum，恰好是 reduce 契约可逆性条款点名安全的聚合（「count、sum、min/max 合格；median/distinct 不合格」）。这里没有框架需要保留的用户自由度——只有重复。
3. **Max 有一个值得一次性编码的微妙点。** 当被聚合的量是分配出的标识符时（id 永不复用、watermark 不可回落），max 的 `unfold` 必须是 no-op；但当它是可缩小的值时（删除行之后最大值**应该**回落），必须是真正的 un-max。两者都正确；用哪个取决于**字段的含义**，不是每个用户都该从 fold/unfold 契约里重新推理一遍的东西。
4. **bindings 也会需要。** ADR-0022 赋予宿主语言 callable 实现 `ReduceLogic` 的能力。预置库为标准累加器（`u64` BE 等）提供字节级精确的参照——动态侧可以声明式地用 `count()`，不必重新推导 codec。

## 决策

**okm-core 在 `ReduceLogic` 旁边提供预置组合子库：标准累加器一行声明、零样板、同样的写路径机制。**

- **`Count`**——每组行数。Acc `u64`；fold +1；unfold −1。
- **`Max<F>` / `Min<F>`**——每组一个数值 payload 字段的极值。Acc `u64`；fold 对累加器取 max/min。
- **`Sum<F>`**——每组一个数值 payload 字段的总和。Acc `u64`；fold 相加；unfold 相减。
- 声明骑在既有属性上：`#[ok_reduce(Count { group(user_id) })]` /
  `#[ok_reduce(Max::<F> { group(type_id) })]`——derive 把组合子解析到它的 Reduce impl，与解析用户写的 logic 类型完全一样。无新属性、无新 wire 格式、无新 slot 规则（slot 继续按声明顺序计数器，ADR-0016）。
- **幂等恒等变体是独立名字，不是开关**：`HighWater<F>`（unfold = no-op——用于分配标识符的 watermark）与 `Max<F>` 是不同的组合子。unfold 的歧义在声明处用**名字**解决，schema 里可见，而不是埋在类型参数里的布尔。
- 组合子住在 `okm_core`（`model/reduce.rs` 或相邻模块）——是库代码，遵守与 `ReduceLogic` 本身相同的无 `VirtualStorage`、引擎无关纪律。derive 除接受这些名字外无改动。

### 这里不裁决的

- **不做内置表统计。** 行数、每表 max/min 不进引擎：那会让所有表的写路径为一个多数表从不读取的能力交税，而且「要哪个聚合」是用户的决策，框架无权代做。分界线：定义上必然存在的机制（今天没有——连行数都是一种选择）不内置；用户选择的语义经声明到来。
- **`row_count()` 便捷读取离一个 reduce 声明的距离。** 想要行数的表声明一行 `Count`。若某天读取频率足以证成零声明计数，那是另一条裁决、有它自己的写路径定价——本 ADR 不预设它。

## 诚实的语义代价

- **`Sum` 在覆盖写下不满足顺序无关**（浮点）；预置只收整数（`u64` payload 字段）。浮点求和仍是用户手写 `ReduceLogic`（用户可以在那里选补偿策略），不做静默有损的预置。
- **`Max`/`Min` 的 unfold 本性有损**（删除当前最大值无法在不引入第二结构的情况下恢复前一个）。因此 （若提供的话）`Max<F>`/`Min<F>` 要求可逆恒等式读法：组里存的值是上/下界，删除后可能合法地低于剩余行的真实极值。需要精确的消费者用手写 logic（或接受有损上界读法） 配一个 `Count` 检测过期。这是契约既有可逆性条款的具体化，不是新让步——但预置让它更容易被无意踩中，所以写明。
- **key 侧量的镜像字段问题由 ADR-0024 解决**（reduce 钩子接收解码后的 key；GROUP 可引用 key 字段）——记录在该 ADR，因为那是钩子契约变更，不是组合子关切。0024 落地后，`Max<F>`/`HighWater<F>` 按名聚合 key 字段，镜像模式退役。

## Consequences

- 标准聚合缩到一行声明；常见形状的 fold/unfold 仪式消失。
- 标准形状的可逆性与溢出语义被一次性编码、一次性评审、一次性测试（okm-core 的 reduce 测试矩阵纳入预置用例），不再每个用户重新推导。
- bindings（ADR-0022）拿到一组固定、字节级成文的累加器，可以声明式地暴露。
- 实现：组合子是泛型 `ReduceLogic` impl + derive 按名接受；测试覆盖进 `reduce_test.rs`；文档过一遍 INTEGRATION/MODELING。wire 格式、slot 分配、引擎契约零改动。
