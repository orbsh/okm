# 索引机制：条目、扫描与回表

本文是机制与实现细节文档：`#[ok_index]` 声明展开成什么、条目怎么编码、`scan::<I>` 与 `scan_covered` 怎么工作。建模纪律见[建模指南](../MODELING.zh-CN.md)（访问方法强制、覆盖索引克制）；slot/ns 推导见[slot 机制](slot-mechanism.zh-CN.md)。

## 声明展开

`#[derive(DocumentEncode)]` 对每个 `#[ok_index(name { ... })]` 生成一个**索引类型**（marker struct，生成在展开点）：

```text
__OkmIndex_{Row}_{name}      // 机械拼接，无大小写转换
```

类型实现 `KvIndex` trait（`okm-core/src/model/index.rs`），携带编译期常量与方法：

```text
type Key / type Document     关联的主键与文档类型
const SLOT                   声明序序位（1, 2, …；0 = 主表），4 字节头后 2 字节 BE
const FIELDS                 索引字段（排序序）
const INCLUDES               覆盖字段（进 value，不参与排序）
const KEY_PREFIX             主键尾段截断子集（空 = 全长）
const FUNC                   函数索引路径（空 = 普通字段索引；文档性常量）
fn encode_named(...)         按字段名序编码 payload 段
fn admits(...)               部分索引谓词（默认 true = 全量索引）
```

查询侧把类型当泛型参数：`t.scan::<ByOrg>(&prefix)`；`use __OkmIndex_User_by_org as ByOrg` 只是书写别名，不参与机制。

## 条目编码

```text
entry key   [ table_ns 2B BE ][ slot 2B BE ][ 索引字段 BE ][ 主键前缀 ]
entry value [ includes 字段 TLV ]                （无 includes = 空）
```

- **索引字段段**：`FIELDS` 按声明序逐个编码，首位 = 分组维度（最左前缀匹配的物理基础）。**至多一个变长字段且必须紧贴主键前缀之前**：定宽段从右往左依次切分（主键 `KEY_LEN` 提供右手锚点），剩下整块是唯一变长段，长度由右侧定宽边界反推、不需存储；两个变长段之间无边界字节，derive 编译期拒绝（MODELING「数据段」节）。变长段不在末位则前缀语义破坏（裸字节无终结符，`"beijing"` 命中 `"beijing2"`）。
- **主键前缀**：默认全长主键编码（`KEY_LEN` 编译期锁死）；`key(...)` 声明截断到命名子集。尾段永远可解——它就是主键编码，布局在声明系统内。
- **函数索引**（`func(path)`）：`path` 返回值的每个元素经 `IndexFuncResult` 编码为一条 entry 的数据段——返回形态是 `IntoIterator`（ADR-0055），单值写成单元素数组（一条 entry，经典函数索引如归一化），`Vec<V>`/切片/迭代器一行展开为 N 条（多值 regime：tokenize 倒排、多值字段、时间分桶），空迭代器 = 该行不产生任何 entry。查询侧探针调用同一路径，一条声明驱动两侧。展开细节见[函数索引机制](func-index-mechanism.zh-CN.md)。
- **部分索引**（`where(path)`）：行级谓词，`path(&document)` 为 false 的行在本索引里不存在（Postgres 式 partial index）。两种索引形态都可用——`fields(...)` 与 `func(...)` 各自叠加谓词，谓词读哪些列不受限制（不必是索引字段）。

## 尾段的三个职责

条目尾段默认携带全长主键（`key(...)` 可截断到命名子集），三件事同时挂在它身上，缺一件这套布局就不成立：

1. **条目唯一性**：一行一条，`put` 整体替换条目，`purge` 由存储行重新生成整组条目，因此没有读-改-写、没有墓碑。这一条最容易被当成理所当然，直到索引字段出现重复值——见下节。
2. **不读 value 就能拿到身份**：尾段零拷贝切出 `PrefixKey`，探针/尾段判据因此不额外花读；截断的 `key(...)` 前缀仍然可信，是关系表反向查继续回主表的出口。
3. **组内有序**：等值前缀内尾段就是主键字节本身，所以一个等值组的命中按主键序到达。回表仍然是逐条点读——有序买到的是这批点读在主表上单调前进（邻近读、块序递增），不是归并。组外不成立：跨 leading 字段的区间按被区间的维度排，`func` 多值索引把一行重复到多个值下，`key(...)` 截断让尾段并列（[建模指南「设计约束对性能的影响」](../MODELING.zh-CN.md)）。

### 重复索引字段：三种存法，各欠一笔账

索引字段不要求唯一，同值多行是常态而不是退化情形：10 行里 `gender` 只有两个取值，每个取值 5 行。条目键里不含身份，这 5 行就落在同一个键上。

- **一键一值**——5 次 `put` 覆盖同一条目，索引里只剩最后一行。静默丢行，且无法靠纪律禁止：声明本来就没要求索引字段唯一，编译器也无从检查。
- **值里放 id 集合**——每次 `put` / `purge` 都是共享条目上的跨行读-改-写。这个形状已有归属：`#[ok_reduce]`（group 在键、累加器在值、恰好一次 fold/unfold），派生条目不再造第二份。
- **键里加 `seq`**（`[gender][seq]`）——5 行确实拿到 5 个键，但 `seq` 是存储分配的地址，不是行持有的值，代价三笔：写侧每个 group 维护一个计数器（本身又是一次读-改-写）；删侧要能从行重建地址，因为删除集合由存储行派生（`purge` 内部 get）而行里没有 `seq`——要么把 `seq` 写回行，要么另建 `(group, pkey) → seq` 的映射；组内顺序退化为插入序，尾段不再是主键字节，回表从顺序归并退成逐条点查。

把身份放进尾段，这三笔都不欠：区分位就是行本来带着的值，覆盖写算出同一个键，`purge` 用同一份行重新生成同一组键，一次前缀扫拿到的就是整个 group 且按主键有序。省下的不是一个字段，而是**让条目地址保持为行的纯函数**——`entry_pairs` 是唯一生产者，写入集合与删除集合同源，索引没有运行时簿记（见「写路径的同步」）。

## 部分索引：谓词唯一的作用点是 entry_pairs

`where(path)` 展开成 `KvIndex::admits` 覆盖：

```text
fn admits(document) -> bool { path(document) }      // 无 where → trait 默认 true
```

写侧唯一的条目生成点是 `entry_pairs`（put / delete / save_into 都经它），谓词在函数开头短路：

```text
if !Self::admits(document) { return Vec::new(); }
普通索引走 trait 默认实现（同样带这句），函数索引用同一句再走 fan-out
```

由此三条性质：

- **对称性**：写入集合与删除集合同源（同一个 `entry_pairs`），谓词为纯函数时 delete 恰好清掉 put 写的那些条目。
- **谓词按行求值一次**：fan-out 索引不会把谓词乘以 N 条 entry。
- **读侧零新增机制**：部分索引只是更稀疏，`scan::<I>` 不变；谓词不进 entry key（key 仍只由索引字段与主键决定）。

谓词翻转（行从被接纳入索引变为被拒绝）不在谓词的作用范围内：条目由索引字段决定地址，谓词只决定"这次写不写"，所以 `put` 覆盖写不会清理旧条目（与「值变化留下悬挂条目」是同一既有契约，见[建模指南](../MODELING.zh-CN.md)）。删除集合由**存储的那一行**派生（`purge` 内部 get），所以清理悬挂条目需要"生成它的那一行"，而 2026-10-23 起没有任何动词收它——覆盖写走 `merge` / `replace`，它们在替换前对存储行扫面。

## scan::<I>：前缀扫描 + 回表

```text
t.scan::<I>(&encoded)
  1. entry_prefix = [ ns 2B ][ slot 2B ] + encoded     编译期选表与序位，运行期拼字节
  2. store.scan_suffix(&p)                             引擎范围扫，返回去前缀的 suffix
  3. 长度判据丢弃：suffix.len() < taken 的条目不是命中（探针字节被主键尾段满足）
  4. 字段段 → I::Fields                              derive 生成的解码器（定宽切、末位变长吃剩余）
  5. 尾段切出主键编码 → PrefixKey<K> { decoded, taken }   宽度已知（key_prefix_width）
  6. 回表：taken == KEY_LEN → get(decoded) + decode_payload
          taken <  KEY_LEN → row = None（截断前缀无法回表）
```

返回 `Vec<(I::Fields, PrefixKey<K>, Option<R>)>`。第一个元素是**这一条按之排序的那一段**解出来的值：
`fields(...)` 声明得到 derive 生成的字段结构（每个字段按行里的类型解码），`func(...)` 索引的段是
函数输出、声明无从命名它的类型，于是交回 `RawFields`（裸字节），`ok_lead` 面同样是 `RawFields`
（那是键自己的字节，键类型已经能解码）。它是为了不让调用方"按某列扫描、却还要回表或加
`includes` 才读得到那一列"——按索引字段做 join 因此不需要任何额外读。

截断 `key(...)` 的索引只能拿到可信前缀字段，调用方拿 `decoded` 去主表继续前缀扫描。

**第 3 步是这一族的唯一判据。** 字段段与主键尾段之间没有边界字节，所以探针的字节可能被"更短的值 + 尾段"凑出来（存 `"ad"`、尾段首字节 `0x31`，探针 `"ad1"`；值与主键都允许字母数字混用，这是合法数据，不是可以靠纪律禁止的输入）。尾段宽度已知，于是字段段长度 = `suffix.len() - taken`（含探针自身）：**剩余字节短于尾段宽度 ⇒ 字段段比探针短 ⇒ 值不可能以探针开头**，丢弃。这个判据只在扫描侧做一次长度比较，不回表、不读 value；"更长的值也命中"不属于假命中（那是前缀语义本身），只有这一族是。

回表是每个命中一次点读，帮上忙的是命中**按主键序到达**：条目按 `[索引字段][主键前缀]` 聚簇，主表按 `[主键]` 聚簇，于是**等值前缀固定 leading 字段时**，这一组的点读在主表上单调前进（递增块序），而不是散开。扫描序仍然先按索引字段——只有在一个固定的字段元组内部才按主键，所以这是局部的有序，不是「主键有序的一条流」：跨 leading 字段的区间读、只钉部分 leading 字段的组、多值 `func` 索引、`key(...)` 截断都不满足它。需要归并的地方由算子自己排序（`okm_query::merge_join` 收下两侧后排序，已有序只是省掉那次排序），所以这些形状不会算错，只是省不掉排序、局部性论证也不适用。量化分析见[建模指南「设计约束对性能的影响」](../MODELING.zh-CN.md)。

## scan_exact：全字段等值

同一个长度量换个比较：`suffix.len() == taken` ⇒ 探针吃尽整个字段段 ⇒ **值恰好等于探针**。

```text
t.scan_exact::<I>(&encoded)
  1~2 同上（引擎扫描范围与前缀扫完全一致）
  3. suffix.len() == taken ? 保留 : 丢弃   —— 更长的值（前缀扫的命中）与
                                          "探针啃进尾段"两侧同时被排除
  4~5 同上（尾段解码 + 回表）
```

它是**不回表的精确匹配**：不需要行校验，也不需要 `includes(该字段)` 来核对。前提是探针给出该索引数据段的**全部字段**——只给前导字段是分组语义（属 `scan::<I>`），等值形态下这些分组命中全被判掉。

动态侧是同一判据的逐字对应：`okm_dynamic::scan_access_method` / `scan_access_method_exact` → `DynamicCollection::scan` / `scan_exact` → Python binding 的 `Collection.scan` / `scan_exact`（`accept_embedded.py` 锁一条 spill 断言）。

## scan_covered：覆盖扫描

`includes(...)` 字段复制进条目 value，`scan_covered::<I>` 免回表：

```text
store.scan_suffix_kv(&p)            连 key 与 value 一起取
  → 尾段仍从 key 切出（宽度同上）
  → value 段按宽度切出 includes 字段（裸拼接，无 tag、无长度前缀）
```

返回 `(I::Fields, PrefixKey<K>, I::Covered)`——字段段与回表扫描同源，`Covered` 只描述条目值。`Covered` 是 derive 为每个带 includes 的声明生成的
结构体 `__OkmCovered_{Row}_{索引名}`：字段与 `INCLUDES` 同序、同 Rust 类型，所以读侧
拿到的是字段值，不是字节。变宽字段只能排最后（与 `fields` 同一条声明期规则），解码时
它取该条 value 的剩余字节；没有 includes 的声明 `Covered = ()`——没复制，也就没有可解的。

动态侧的对应物是 `DynamicCollection::scan_covered`，返回 `(key 字段, 覆盖字段)` 两张
名字映射（分开返回：key 字段名与 payload 字段名可能撞，而 key 是身份不是 payload）。

它是高扇出查询的物化视图——每次 put/delete 付永久写成本（每个条目多 |includes| 字节；
条目本来就整体重写，所以 op 数与 commit 数不变），纪律见[建模指南「覆盖索引克制」](../MODELING.zh-CN.md)。

## 写路径的同步

`Table::put` 单次写入：主表条目（slot 0）+ 每个声明索引的 `entry_pairs()` 全部条目（普通索引一对；多值函数索引 N 对），同一 store 实例内；`purge` 对应删除全部。没有运行时索引簿记——`index_entries()`（derive 生成）静态展开为每个索引调用 `entry_pairs` 并展平，声明即注册。**因此条目与声明永不失配**：库里存在哪个 ns 段的条目，当且仅当源码里声明了对应索引。覆盖写时 reduce 账本的 unfold/fold 补偿是另一条约束，见[reduce 机制](reduce-mechanism.zh-CN.md)——索引条目随覆盖自然转移（旧条目悬挂只在 `merge` / `replace` 的扫面里兜住，裸 `put` 的覆盖没有兜底），账本必须就地平账。

## 两种"一对多"的分野

同表 payload 维度 → `fields(...)`（本文机制）；跨表实体关系 → `JunctionEncode` 双向边（见[建模指南「多对多关系」](../MODELING.zh-CN.md)）。二者机制同构（次级 key 布局指向身份），分界在数据源：索引 = 本行 payload 的派生视图，边 = 一等的关系数据。
