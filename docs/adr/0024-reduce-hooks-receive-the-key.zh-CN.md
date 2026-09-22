# ADR-0024: reduce 钩子接收解码后的 key——fold(acc, key, item)

> **Languages:** [English](0024-reduce-hooks-receive-the-key.md)（主文档） · [中文](0024-reduce-hooks-receive-the-key.zh-CN.md)

**状态**：已接受（2026-09-22）；实现待排期，见 Consequences。修订 ADR-0023 的「不在此夹带」注记（Honest semantic cost 第三条）：key 侧量问题由本 ADR 解决，0023 的预置组合子在本 ADR 落地后即可聚合 key 字段。

## 背景

reduce 钩子契约（ADR-0008）是 `fold(acc: &mut Acc, item: &Self::Document)`——累加器只折叠 **payload 字段**。分组段（GROUP）同样由 payload 字段经 named-field walk 编码。主键（`Key`）对钩子不可见。

第一个生产消费者（aura 的 state 表，ADR-0018 step 2）正面撞上这个约束：被聚合的量——每 type 的 proxy-id watermark（id 分配用）——住在 **key** 里（`InstanceStateKey { type_id, instance_id }`）。fold 看不见 key，payload 只好带一个镜像字段（`instance_id` 写两处：key 一份、payload 一份），reduce 实际聚合的是镜像。三个代价：一个值两个真相源、冗余字节、一条读侧规则（「寻址走 key，payload 那份只是给钩子看的」）。

### key 本来就在手上，本来就是解码后的

钩子链路从不缺 key：

- 写侧每个调用点（`Collection::put` / `delete_by_pkey` / `upsert_with`）调的都是
  `R::__okm_apply_reduces(&mut self.store, key, document, &header, add)`——生成钩子的签名里**本来就有** `_key: &Self::Key`，只是 derive 没有把它继续传给 fold。
- 那一刻的 key 是**调用方手上的解码后类型化引用**（`put(key: &K, ..)` 自己的入参），不是裸字节。`Key::decode` 只在 index 一侧使用（从 entry 尾字节还原 key）；reduce 写路径不在那条路上。

所以「把 key 传给 fold」= 把一个已有的对象引用多传一层——零序列化、零解码、零拷贝。用户的表述成立：item 本来就是未序列化对象的引用（`&Document`）；key 以同一形态加入（`&Key`）。

### group 字节引用 key 字段不需要新编码

`KvIndex` 已经定义了双源 named walk：`encode_named(key, document, names, buf)`——index 的 `fields()`/`includes()` 本来就可以引用 key **或** payload 字段，按来源各自编码。reduce 的 group walk（`__okm_encode_named(document, GROUP, buf)`）是同一模式的「仅 payload」半边。把 GROUP 放宽到 key 字段，等于把每个声明名路由到它来源的编码器——index 层的字节布局（以及 dynamic 侧的 `ReduceSpec.group_bytes`，今天显式拒绝 key 字段名的那个 `Err`）就是参照。

## 决策

**`ReduceLogic` 的钩子接收解码后的 key 类型化引用：`fold(acc: &mut Acc, key: &Self::Key, item: &Self::Document)`，unfold 同。GROUP 声明可以引用 key 字段；每个 group 字段从自己的来源编码（key 走 `KeyEncode`，payload 走 payload walk）——即复用 `KvIndex::encode_named` 的双源规则。**

- **每个点都是引用，不是字节。** 写路径本就持有 `&Key` 与 `&Document`；钩子改动是一次参数传递，绝不是解码。（`Key::decode` 仍然只属于 index 扫描侧。）okm-dynamic 孪生侧同样传解码形态：document 本来就是 `&ValueMap`，key 按 schema 解码成 key 字段 map——绝不向宿主语言 callable 递交字节。
- **GROUP 名字解析变双源。** `#[ok_reduce(Logic { group(a, b) })]` 可以引用 key 和 payload 字段；校验（schema.rs、okm-dynamic 的 `ReduceSpec`）撤掉 key 字段拒绝，按来源路由。字节兼容性：group 段布局规则不变——声明名按顺序、各字段定宽 BE 编码；唯一变化是**每个名字的编码器来源**。一个名字在两个来源同时存在 = 编译错误（来源歧义；「payload 静默获胜」的惯例被否——歧义不该被悄悄消解）。
- **累加器形状零改动。** `ReduceCodec`（`u64` BE、`Vec<u8>` 逃生口）不动；ADR-0023 的预置组合子按名声明聚合字段，这个名字现在可以是 key 字段——标准形状下的镜像字段模式就此消失，无需组合子层面任何特例。
- **exactly-once 不变。** fold 仍在同一引擎锁内读-改-写里、由同一批调用点喂；多传一个已有的引用不改变原子性表面（ADR-0008 的不变量关于状态可见性，不是签名）。

## 诚实的语义代价

- **每个 `ReduceLogic` impl 的签名 breaking。** 仓库内：`reduce_test.rs`（`AuthorStats`）、`subscribe_common.rs`（`CounterTotals`）、aura 的 `MaxInstanceId`——机械地加一个 `_key` 前缀参数；迁移说明进 changelog。reduce 的仓外用户（该特性尚年轻）付同样的一次性改名。
- **okm-dynamic 的 `ReduceLogic` trait 同批变更**（`fold(&self, acc, key, document)`）：dynamic 的 key 形态在此一并裁定——是 schema 驱动解码出的 key 字段 map，不是裸字节，宿主语言 callable 看到的是统一的对象模型。Python `add_reduce` 的 callable 同版加 key 参数。
- **歧义规则是编译错误，不是消解。** group 字段名同时存在于 key 与 payload 两个 struct = derive 失败。备选（payload 静默获胜）会让 group 编码依赖被遮蔽的名字——与文档 get 路径对 declared/dynamic 冲突的记载同类陷阱。
- **`scan_reduces` 返回 group 段，不解码 key。** 由 key 字段构成的 group 段调用方可自行解码（字段宽度在 schema 里），但读侧助手不返还类型化 key——那个便利若需要，是独立后续，不夹带进签名变更。

## Consequences

- 镜像字段模式退役：被聚合的量住在它该在的地方（key），单一真相源，钩子可见的重复消失。
- ADR-0023 的组合子按名聚合 key 字段（`MaxKeep::<instance_id>` 形状的用法）——aura 手写的 `MaxInstanceId` 收缩为一条预置声明。
- 钩子签名在 reduce 采用尚年轻时到达它的终态；再晚会倍增迁移成本。
- 实现：okm-core trait + derive 透传 + 双源 group 校验（schema.rs、okm-dynamic `ReduceSpec`）+ okm-dynamic/Python callable 签名；测试扩展 `reduce_test.rs`（key 字段 group、key 字段 fold）与 dynamic 语义测试；文档过 INTEGRATION/MODELING。wire 格式、slot 分配、entry 布局零改动。
