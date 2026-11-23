# 声明编译成什么，以及它写进哪个地址

> **Languages:** [English](plan-and-generation.md) (primary) · [中文](plan-and-generation.zh-CN.md)

机制/实现文档：一份声明编译出的东西，以及编译结果住在哪——plan（路由）、条目布局（生产者）、capture 过滤器，以及 layout generation 拥有的那个地址。决定在 [ADR-0074 §3](../adr/0074-the-store-describes-its-tables.md)（一张表一份 plan）、[ADR-0075 §2](../adr/0075-a-generation-change-is-a-migration.md)（generation 是地址的一位）、[ADR-0076 §1](../adr/0076-the-mechanism-moves-under-the-declaration.md)（条目布局是唯一的条目生产者）、[ADR-0056 §2](../adr/0056-the-segment-is-the-migration-path.md)（日志带什么）、[ADR-0042](../adr/0042-layout-versions-are-stamps.md)/[ADR-0043](../adr/0043-table-meta.md)（印记）。数字本身在 [key layout](key-layout.zh-CN.md)；条目在 [index mechanism](index-mechanism.zh-CN.md)；段的字节在 [dynamic segment](dynamic-segment.zh-CN.md)。English: [plan-and-generation.md](plan-and-generation.md).

一份声明产生四样东西，它们很容易混在一起。**地址**是这张表的行住在哪里——它的 ns，其中一位属于 layout generation。**plan** 是路由需要的东西：键前缀、日志带哪些槽、声明给了它们什么编号。**条目布局**是一行必须产出什么。**印记**是 store 记住前两样的方式，只写一次，落在表的 meta 行里。它们在不同时刻构建、由不同所有者持有，声明变化时也以不同方式移动。

## 地址：身份、地址，以及两者之间的那一位

一个键是 `[partition?][ns 2B][slot][payload…]`。ns 是两个字节，其中只有十五位用来命名这张表：

```rust
/// The bits of an ns that name the TABLE: the rest belong to the layout generation
/// (ADR-0075 §2).
pub const IDENTITY_MASK: u16 = 0x7FFF;
```

第一字节的最高位是 generation 的奇偶——`LAYOUT_VERSION & 1`——所以 `ns < 0x8000` 是被检查的前置条件，不是注释（`document.rs::ns_bytes` 用带 ADR 编号的信息断言它）。身份与地址是两个不同的问题，mask 就是值在两者之间跨越的那一处：名字、成员列表、`#[ok_nodes]` 里的条目和 `Collection::ns` 说的是身份；键说的是地址。

有三处构建者会设置这一位，它们必须逐字节一致：

- **derive**，在编译期：`NS_PREFIX` 是 `&[ns_hi | parity, ns_lo]`，其中 `parity = (layout_version & 1) << 7`（`okm-derive/src/document_encode.rs:1550`）。typed 键从这个 const 起步，所以这一位在任何运行期代码跑之前就已经在里面。
- **`ns_bytes::<R>(ns)`**，给两个运行期接收 ns 的构造器（`with_ns`、`rebased`）：同样的 OR，所以一张被映射的表仍然带着**这个**声明的 generation。
- **`TablePlan::compile`**，给从 store 读回来的声明：它先用 `0x7F` 掩掉存着的 ns，再 OR 上自己的奇偶，于是以字节形式到达的 schema 与编译期声明逐字节写出同一个地址。

翻转**不**移动的东西：槽编号、payload 宽度、覆盖它们的面指纹。这正是 generation 做成地址的一位、而不是每行一个版本号的全部理由——新一代是同一张表在另一个地址上，所以迁移是一次拷贝加一次重放，不是重写上一代已经写下的字节（ADR-0075）。

## plan：只有路由

`TablePlan` 只有三个字段，回答三个问题（`okm-core/src/model/table_plan.rs`）：

```rust
pub struct TablePlan {
    header: &'static [u8],            // [partition?][ns 2B]，就是上面的地址
    sides: [u64; BITMAP_SLOTS / 64],  // 一槽一位：置位 = 日志带这个槽
    slots: SlotMap,                   // 声明的编号，给位图放不下的那些
}
```

`header()` 是写操作要前置的字节。`carries(slot)` 回答 capture 那一问：`0x0100` 以下一次测试一次移位，以上再用规则问一遍，因为位图只覆盖固定带与标准家族（`BITMAP_SLOTS = 256`）。`carries_key(key)` 是同一问题问在字节上：剥掉表头（跨表 batch 里别的表的键不归这张表写）、按位置读出槽（`FIXED_FACE_MAX = 0x1F` 及以下一字节，以上两字节）、拒绝裸前缀（词表的光标和 meta 行是单例，没有行长这个样子），然后走 `carries`。

两个构造器，一个形状：

- `TablePlan::of(header, slots)` 是 `const`：没有东西要构建，没有东西要锁，也没有第二份副本会漂移。derive 按 ns 把一份 plan 发进 `static __OKM_NS_{ns}_ENTRY: OnceLock<TablePlan>`，attach 用 `get_or_init(compile(declaration))` 填一次（ADR-0079 §2）。同模块里第二个行声明同一个 ns 就是编译错误，错误信息里带 ns。
- `TablePlan::compile(&schema)` 是动态路径：没有类型可问，所以同样的值在运行期从 store 的 meta 行里那份声明构建出来。表头是泄漏的——每份不同声明两字节——因为 plan 天生 `'static`，替代方案是给每张表都加一个生命周期参数。

plan **必须不会失败**：`compile` 返回 `Self` 而不是 `Result`，因为日志对每张表只持一份，并且在每次 stage 时问它（`VirtualStorage::note_table` 收 `&'static TablePlan`；日志里是 `Known { meta, plan }`）。被如此广泛共享的结构承载不了「按句柄拒绝」——这正是条目那一半住在别处的第一条理由。

## 条目布局：会被拒的那一半

`EntryLayout` 是同一次编译的另一半，由 COLLECTION 持在它复用的 `Entries` 缓冲旁边（`okm-core/src/model/entry_layout.rs`，ADR-0076 §1）：

- 它**会失败**。`EntryLayout::compile(schema, faces)` 返回 `Result<Self, Unsupported>`：面可能点名 schema 里没有的字段、把变长字段放在非末位、或者段的来源被 `func`/`where` 占了。拒绝出现在声明到达的地方——动态表是 `declare_index`，typed 表是拼装点。
- 它**按句柄**，不按类型。动态表在声明到来时重编它，而它的 plan 的表头、位图与槽号一步不动。
- 它**只有一个读者**，写路径。日志从不问它任何事。

typed 侧把它持成 `Option`，而 `None` 是一个决定而非缺失：带 `func` 面或 `where(...)` 谓词的表，它的**所有**面都交回生成的生产者，因为一个面只能有一个生产者，而 walk 生产不出那一个。

布局里装的是什么——键字段的静态偏移、hot 字段的偏移、cold 字段的 tag，外加每个面的 includes 与尾部——以及 walk 如何把它变成条目，见 [index mechanism](index-mechanism.zh-CN.md)。

## capture：日志带哪些槽

这个划分只写在一处，`changelog::side_of(slots, slot)`，capture 过滤器与 drainer 都读这个答案，而不是各自重打一遍编号（`okm-core/src/changelog/mod.rs:93`）：

- **Truth**——重放推不出来的东西：主行、动态段、四个词表方向（`field_vocab_id/name`、`predicate_vocab_id/name`）、cluster-kind 词表（`0x0A`/`0x0B`，它是字典而非推断）、cluster 行（`0x0C`）与它的 edge 成员（`0x0D`）——分组是一次断言，不是一次派生。
- **Derived**——读者可以把 truth 重放过一次写路径就重建出来的东西：声明的索引、reduce 累加器、lead 面、图的遍历面、cluster 的 child 索引（`0x0E`）与 edge → cluster 索引（`0x0F`）、向量快照、表的 meta 行。
- **家族兜底**——在具名编号以上，答案来自 `slot >> 12` 的家族：`VOCAB_LABEL`/`VOCAB_ID` 是 truth，其余都是 derived。这就是为什么这个分类可以是完备的：已知家族里的新槽号会被回答，而不是从白名单的缝里掉下去。

plan 把固定带上的答案预先算成 256 位的位图，在 `TablePlan::of` 里用声明携带的那十个具名 truth 槽构建。位图以上 `carries` 再问一次 `side_of`，于是快答案与通则不可能不一致——带内规则的答案永远是一个具名槽，带外规则只多一次调用。

## 印记，以及第二个奇偶

store 关于一份声明只记两件事，落在表的 meta 行 `[header][0x1F]` 里（ADR-0043、ADR-0042）：`version`——payload 写在哪个 layout generation 下——和 `faces`——声明出来的面的指纹（ADR-0048）。

`meta::stamp_layout(store, header, layout_version, faces)` 是唯一的写者：读那行（新 store 没有），`check_layout`，写两个名字，put。而 `check_layout` 就是「外来的 generation 被按名字拒绝、而不是被读错」的地方：

```rust
Some(stored) if stored != layout_version => Err(Error::LayoutMismatch { stored, current }),
Some(_)                                  => Ok(()),
None                                     => Err(Error::LayoutUnstamped { current }),
```

首次触碰从不**修改** `faces`：一行存在但没有指纹，要么比 faces 这个机制更老，要么是一次没跑完的重建，两种情况都必须继续回答 `Stale`/`Unknown`，好让修复动词有东西可处理（ADR-0048）。

因为奇偶是地址的一位，一次翻转会留下相隔一位的两行 meta。`meta::current(store, header, this_build)` 两行都探——`other_parity(header)` 翻转表头第一个字节的 `0x80`——取版本更大的那行作为当前。所以两行不是冲突而是事实：一次翻转发生过，新行是活的，旧行等它的 vacuum。

日志跟着同一个地址走。它的目录名是表头的十六进制（`table_dir(root, header)`），所以一次 generation 翻转会把新一代的日志放进它自己的目录。段自己的表头也带 `layout_version`，而一个 window 只装一个 generation：遇到另一个 generation 的段，当前 window 到此结束、下一段另起一个——升级之后这是常态，不是损坏（ADR-0056 §5/§6）。

## 声明变化时什么会移动

| 变化 | 移动的东西 |
| --- | --- |
| payload 形状（`LAYOUT_VERSION` +1） | 奇偶位、地址、meta 行、日志目录；槽编号与宽度不动 |
| 加/删/重编一个面 | `faces` 指纹：`faces_status` 回答 `Stale`，`rebuild_faces` 重新派生 |
| 某个面的 callable 改了 | `AccessMethod.version`（动态）搭乘指纹——不需要谁记得去加一 |
| 什么都没声明 | 什么都不移动；`ensure` 写一次印记，日志的 `Known` 记录是共享的 |

## 数字住在哪

`okm-core/src/model/layout.rs` 把每个宽度和每个固定面收在一个文件里，带子按连续表写在文档里：`0x00`–`0x1F` 固定面，`FIXED_FACE_MAX = 0x1F`，所以 `slot <= 0x1F` 是一字节，以上两字节；`SLOT_COUNTER_BITS = 12`，家族 `0x2..=0xF`。改一个宽度就是改那个文件——其余一切都以它的常量表达。

## 换一代

判定、拷贝、翻转是一个函数 `okm_core::changelog::migrate::migrate_table`，也是一个动词：`okm-log migrate --store <dir> --table <hex> --to <schema.json> [--root <dir>]`。`--table` 是表的日志目录被命名用的那个 header——地址，位在里头——`--to` 是构建导出的下一代声明。

判定问的是 store，不是调用方：`meta::current` 说哪一代是活的；目标若没有把这张表推进一代就拒（它会落到同一个奇偶上）；目标若点名别的 ns 也拒。比两份声明的指纹不属于判定——迁移的前提就是它们不同，那种比较只能答「是」。它在 attach 处发生（ADR-0074 §5），迁错目标也是在那里被抓住的，在服务任何一行之前。

拷贝用 store 记的声明解旧区间的每一行、用新声明写回去，于是两代唯一不一致的地方就是移动过的偏移。然后是尾巴：给了 `--root` 意味着拷贝起点之后封的段要重放在上面（写者一直在跑）；没给意味着调用方静默了这张表，动词会把这条假设打出来。翻转是新句柄写下的那一条记录，什么都不删。

拷贝被校验两次，且两次都在不可逆动作之前。旧区间的每一行都必须在新区间里存在，并且用它自己的声明解出来的值相同：偏移本来就不同，所以比的是解出来的行，绝不是字节。新侧缺一行、某字段值不同、新声明丢掉了某个字段，三种情况各自点名拒绝。`migrate` 在写记录之前跑它，`vacuum` 在删第一个 key 之前跑它——于是翻转与删除这两个唯一收不回来的动作，都压在一次已经通过的比较后面。

`okm-log vacuum --store <dir> --table <hex> [--root <dir>]` 是第二个决定，也是唯一删东西的地方。两条记录里更旧的那条决定删哪段区间；区间的 key 与它的记录一起走；给了 `--root` 连旧代的日志目录一起走——留下一个段会被重放进一个没有记录的区间，那是一张谁也打不开的表。重跑与已完成同样返回。`--dry-run` 跑同一道闸门、算同一份数量，一个 key 都不删——这正是操作者该按的顺序：先看它过不过，再看究竟会删掉什么。这个 vacuum 是 store 的；`okm-lake vacuum` 回收退役版本背后的湖文件，问的是另一个问题。

## 本文不覆盖

键的 payload 段与其顺序（[key layout](key-layout.zh-CN.md)）；条目如何由它们构建（[index mechanism](index-mechanism.zh-CN.md)）；动态段的块表（[dynamic segment](dynamic-segment.zh-CN.md)）；累加器（[reduce mechanism](reduce-mechanism.zh-CN.md)）；槽带的完整表（[slot mechanism](slot-mechanism.zh-CN.md)）。