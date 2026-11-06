# ADR-0054：前导面按身份排序，不按副本排序

Date: 2026-11-06
Status: Accepted（已落地：声明解析、槽族、委托式 `KvIndex` 实现、带 const 检查的类型化探针 builder、免负载重建、`prune_deprecated_slots` 覆盖、导出面、测试）

## Context

ADR-0052 把索引字段锁定在负载来源，并写明了它接受的代价：键里已经有的字段，行上必须再有一列，反查才是一次索引扫描。那个列是**身份的副本**，而模型里没有任何东西要求它与身份相等——`split_names` 把和键同名的字段当作普通值字段，于是走 map 桥的 `merge` / `replace` 可以把那一列写成别的值而键不动。反查给出的就是"列匹配的那些行"，不是"身份匹配的那些行"。对关系表（ADR-0051）来说，这恰好是那个模式唯一依赖的不变量，而它没有承载在任何地方。

ADR-0052 给键取段开的第二笔账是校验：文档派生宏看不到另一个类型的字段，`fields(...)` 无法在展开期检查，探针 builder 也会失去写死的参数类型。这条讲的是错误出现在哪，不是检查能不能存在——`KeyEncode::FIELDS` 是一张 `const` 表（`name`、`ty`、`width` 全部可在 const 上下文读），所以检查可以改在类型检查期做。

于是问题不是"能不能省字节"，是：一个派生结构该从身份派生，还是从身份的副本派生。

## Decision

**1. 新增第四种声明：`#[ok_lead(name { fields(…) [, key(…)] [, deprecated] })]`。** `fields` 命名的是**键**字段，不是负载字段；条目的排序段就是这些字段从键自己的编码里切出来的那几段。`includes`、`func`、`where` 在展开期被拒绝，错误信息说明理由：前导面没有负载那一半——没有东西可覆盖、可计算、可做谓词。

```text
#[derive(KeyEncode)]
struct MembershipKey { channel: u64, member: Uuid7 }

#[derive(DocumentEncode)]
#[ok_ns(110)]
#[ok_lead(by_member { fields(member) })]
struct Membership { nickname: String, since_ms: u64 }
```

**2. 条目就是键，换个槽位。** 段 = `KeyEncode::encode_field_named(key, buf, FIELDS)`——按**声明**的顺序取，不必等于键的声明顺序，因为前导面是按身份的*排列*排序（`fields(member, channel)` 合法，先按 member 排）。宽度 = `KeyEncode::field_width_named(FIELDS)`，尾巴 = 完整主键（或 `key(…)` 的截断，规则与 `ok_index` 完全一致）。因此生成的 `KvIndex` 实现是纯委托，一次都不碰 `Document`：

```text
[ns 2B][slot 2B][member 16B][channel 8B][member 16B]      44 B，尾巴 = 完整键
[ns 2B][slot 2B][member 16B][channel 8B]                 28 B，key(channel)：只出键
```

**3. 同一个 trait、同一批读门、自己的槽族。** 前导面实现 `KvIndex`，于是 `scan`、`scan_exact`、`scan_covered`、区间迭代、`rebuild_faces`、变更日志的"派生面过滤"全部原样生效；`Covered` 恒为 `()`。槽位来自 **`0x5nnn`**（`LEAD_SLOT_BASE = slot(0x5, 1)`），按声明顺序、只追加、`deprecated` 占位保留——与 `ok_index` 同一套纪律，但独立成族，两个族各自编号、各自诊断。

**4. 校验跑在类型检查期，不跑在展开期。** 派生宏解析不出 `MembershipKey` 的字段，所以声明分两处检查，两处都是针对 `<Key as KeyEncode>::FIELDS` 的 const 断言：

- 每个声明字段一条模块级 `const _: () = assert!(key_field_exists(…))`——不是键字段的名字**在声明处**就报错（`E0080`），不必等到第一次探针调用。没有它，一个打错的名字会写出空段：条目仍然存在（尾巴还能区分行），但这张面按什么都没排序；
- 每条探针 step 里再断言 `key_field_matches(…, T::KIND, T::WIDTH)`——这一半需要值的类型，所以落在调用方交出值的地方。step 泛型于 sealed 的 `KeyFieldValue`（`u8…u64`、`[u8; N]`、`Uuid4`、`Uuid7`、`Vocab<N>`）。

`ok_index` 保留它在 `fields(...)` token 上的展开期 `syn::Error`；两个族在不同的时钟上校验，这就是分开的代价，是主动接受的。声明侧失败由 `okm-core/tests/compilefail/lead_not_a_key_field.rs` 锁定。

**5. 面仍然要进指纹。** 前导声明以同一个 `IndexDecl` 形状进入 `INDEX_DECLS` / `FACES`（`fields` 放键字段，`includes` / `func` 留空），槽族就是种类判别。于是老声明写过的库打开时是 `Stale`，而不是静默空着（ADR-0048）。变更日志不用改：它的过滤是"真值面"的正向白名单，新开的派生族天然被排除。

**6. 前导面只靠键就能导出，而且有两处说清这件事。** `KvIndex::entry_pairs_key_only(ns, key)` 是免负载的那一半：默认返回空（负载索引没有文档答不出来，它的 `admits` 谓词也可能读负载），前导面的实现覆盖它，并与文档形那条路共用 `entry_key_with`，所以两种构造不可能给出不同的字节布局。派生宏在此之上把 `Document::key_entries`（前导面调用）从 `index_entries` 里分出来（后者现在从前者开始），`rebuild_faces` 则向**声明**发问「有没有需要负载的面」：全部是前导面的集合修面时不解码任何一行。这就是身份来源段的运维回报——修复路径不会变成负载形状的函数。

**7. 导出面直接说明一张面是什么，而不重复它。** `IndexSchema` 仍是那六个事实；`IndexSchema::kind()` 从槽位族号读出种类（`FaceKind::of`），于是宿主知道 `fields` 里装的是负载字段还是键字段，也不会多出第七列与号位打架。`SlotMap` 增加 `declared_lead_base`（`0x5001`）——宿主唯一无法从面列表里推出的那个数。

## Rejected

- **在 `#[ok_index]` 里加 `source = key` 标记。** 索引的语法是围绕负载长的——`fields` / `includes` / `func` / `where`。前导面一个可选半边都没有，一个属性承载两套互斥制度、两个校验时钟、两种排序域（负载值序 vs 身份字节序），比两个各自只有一份契约的属性面更大。
- **一张固定面加键内的判别字节。** 什么都没买到：前导面每集合是多张，而槽位分类学自己的判据就是"每集合一张 → 固定号，多张 → 计数族"。它还要多付每条目一字节，并给每个读者第二种条目键形状。
- **只保留列副本。** 那等于把漂移风险留在唯一依赖它的模式里——关系表的反向方向，ADR-0051 是按身份定义的。而且这份副本无法被检查：两个事实，没有不变量。
- **等派生宏获得跨类型可见性。** 等不到：过程宏只看得到被标注的那个 item。const 断言是可达的终态，不是以后要替换的权宜。

## Consequences

- **重点不是让反查更便宜——两种写法的条目一样大——而是对键的胃口不同。** 排序能从键里出来，键就可以装扫描要按之排序的不可变维度，而不只是身份；每张声明的面按另一种顺序重读同一批字节，没有负载列需要对账。这是 SQL 主键给不了的余地：那里一个字段进主键就是一次唯一性决定，所以键尽量窄，其余一切都变成副本加索引。边界是不可变性——写入方会改的键段就是搬家——[MODELING](../MODELING.md) 把它写成一节「键装的比主键多」。
- 前导条目是**键的纯函数**：不会陈旧，`rebuild_faces` 重新生成它不需要解码负载（因此对根本没有行的文档也能重建，ADR-0035），负载更新也无法使它失效。最后一条今天是期权不是收益：`put` 仍然重写每一张面。
- 表达域结构性变窄：键是升序定宽，`KeyEncode` 拒绝 `Reverse`（降序）、`String`/`VarInt`（变长）以及各类包装编解码。前导面永远做不了覆盖索引（没有 `includes`），需要 `nickname` 的调用方多付一次 `get`。
- 除非截尾，条目里前导字段会出现两次；截尾改变的是读契约（`taken < KEY_LEN`：只出键、不带行、用前缀续扫接上）。"段来源"与"尾巴宽度"始终是两个旋钮，默认是完整尾巴，这样 `scan` 在两个族上意味着同一件事。
- 两个声明族从此在不同时钟上校验：`ok_index` 打错字是声明处的编译错误，`ok_lead` 打错字是生成实现里的 const 求值错误。两者都是编译期，差别只在报错位置。