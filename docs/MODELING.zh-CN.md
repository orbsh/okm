# 建模指南

如何把领域映射到 OKM 键空间：每实体四层、访问方法强制、覆盖索引克制、主键复合有边界。本指南是应用 schema 的规范性文档；机制细节见各 ADR 与 [README](../README.md)。

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## 四层建模法

每个实体按 **命名空间 → 主键 → 排序字段 → 访问方法** 四层建模。

1. **命名空间（ns）**——实体活在哪个键空间。一类实体 = 一个 ns。2 字节 ns 已经区分了所有条目，不要把实体类型编码进 key。
2. **主键**——身份，且仅是身份。代理键（自增 id、UUID）是默认选择：属性是 payload，不是身份。
3. **主排序字段**——key 的第三段（`[ns][pkey...][order]`）。放 key 尾使同一 pkey 下条目物理相邻且有序，前缀扫描即时间线/区间读；其余排序维度不进 key，经 `#[ok_index]` 暴露。物理决策与间接成本的关系见下文「设计约束对性能的影响」。
4. **访问方法**——声明的 `#[ok_index]` 条目。每一个都是对"这个实体被怎么查？"的常备回答。

说不出访问方法，模型就没建完——说不出的查询会变成全表扫描。

## 声明基础

四层建模法在代码里的落点就是三个派生宏。完整声明词汇：

### 端点 key：`KeyEncode`

```rust
use okm_core::KeyEncode;

/// org 内的用户。org_id 是"组织前缀"，user_id 才是身份终点。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct SessionKey {
    pub org_id: u32,
    pub session_id: u64,
}
```

`UserKey { org_id: 7, user_id: 101 }` 的物理布局：

```
[ org_id: 4B BE ][ user_id: 8B BE ]   = 12 字节，零填充
```

主键定宽：字段按声明序大端编码，`KEY_LEN` 编译期锁死。变长字段见下方「行与索引」——key 保持定宽，变长是索引条目的属性。

### Junction：`JunctionEncode`

```rust
use okm_core::{JunctionEncode, Ref};

/// user → sessions junction（SQL 多对多连接表）。
///
/// 字段引用文档类型：derive 反查 `<User as Document>::Key` 与 `NS_PREFIX`——
/// ns 只在文档上声明一次（User 挂 `#[ok_ns(1)]`，Session 挂 `#[ok_ns(2)]`），
/// junction 自身不声明 ns。
///
/// 正向：user 的身份是 (org_id, user_id) 两个字段 → ok_head(org_id, user_id)
/// 反向：session 的身份是完整 SessionKey（无 ok_head）
///
/// 同一条 junction 的两个方向使用不同宽度的端点身份——
/// 这就是"主键随方向变化"的表达。
#[derive(JunctionEncode, Clone)]
#[ok_junction(1)]
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}
```

`#[ok_junction(n)]` 设置段 0x3 区分号，区分同一端点对上的多条 junction。`#[ok_head(field, ...)]` 声明该端点**在这条 junction 里**哪些字段算身份；不标注 = 全量 key 即身份。字段名必须是端点声明序的前缀（宏生成的编译期检查）。一次声明自动生成每个端点 ns 里的一条单向条目（方向由条目所在的 ns 承载——见下文「多对多关系」）。

### 行与索引：`DocumentEncode`

```rust
use okm_core::DocumentEncode;

/// 行 struct 挂在 UserKey 上（#[ok_ref]）；payload 字段 TLV 编码。
/// 每个 #[ok_index] 声明一个对 PAYLOAD 字段的访问方法——
/// 身份归 key（代理 id），业务维度归行。
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_ns(1)] // 表的命名空间——声明在行上，不声明在 key 上
#[ok_index(by_reputation { fields(reputation) })]
#[ok_index(by_org { fields(org_id, created_at), includes(bio_len) })]
pub struct User {
    pub org_id: u32,
    pub created_at: u64,
    pub reputation: u32,
    pub bio_len: u16,
}
```

- `#[ok_ref(UserKey)]`——行挂到哪个主键上；身份归 key，业务维度归行。
- `#[ok_ns(1)]`——表的命名空间段，声明在**行上**（行是表的声明点：`#[ok_ref]`
  已把 key 类型钉死，行完全决定 `Collection<S, K, R>`）。key 类型不带 ns——同一个
  key 形状可以合法服务多个行/表，各挂各的 ns 号。`Collection::new(store)` 不收 ns
  参数，拼装点只选 engine。junction 不声明 ns——端点文档各自携带（JunctionEncode）。
- `fields(...)`——排序/分组的 payload 字段，按声明序，首位 = 分组维度。
- `includes(...)`——覆盖索引，复制 payload 字段进 entry value（上文「覆盖索引克制」）。
- `key(...)`——把 entry 尾部携带的主键截断到命名子集（默认取满）。截断改变的是行级唯一性，不是分组：`fields` 前缀驱动排序，key 尾段区分行；`key(user_id)` 仅在命名子集对每行唯一时才安全，否则行会互相覆盖 entry。
- `func(path)`——函数索引：数据段由 `path(&row)` 的返回值编码，代替 fields 字段。函数是普通 Rust fn（业务代码里实现），查询端探针调用**同一条声明的同一路径**归一化——编码与扫描共享一条定义，归一化逻辑（lowercase、编码变换）不可能在两侧漂移。一条声明驱动两侧，声明即注册。
- `where(path)`——部分索引：`path(&row) -> bool` 为 false 的行在本索引里不存在（partial index）。谓词读整行，读哪些列不受限制（不必是索引字段）；常规字段索引与函数索引都可叠加。
- 原始字节字段：`Bytes`（`okm_core::Bytes`）——变长冷段 TLV 帧，与 `String` 同一编码、去掉 UTF-8 约束（hash、密文、序列化 blob）。退役拼写 `Vec<u8>` 在编译期被拒绝并指向本条：列表形状的名字错描述了字节串语义。

**基础用法一：单值函数索引（写侧预计算）。** 排序段 = 函数返回值的编码，一行一条 entry，entry 仍随行生灭：

```text
fn lower_name(row: &Doc) -> String { row.name.to_lowercase() }

#[ok_index(by_name { func(lower_name) })]   // "Apple"/"APPLE" 归一化后同位
```

**基础用法二：多值函数索引（一行展开为 N 条 entry）。** 返回 `Vec<V>` 时一行 fan out 成 N 条，token 即数据段（变长、贴主键前），读路径与普通索引完全相同（`scan::<I>`）：

```text
fn hour_bucket(row: &Post) -> Vec<u64> {
    vec![row.created_at / 3_600_000]          // 毫秒时间戳 → 小时桶
}

#[ok_index(by_hour { func(hour_bucket) })]   // scan 传桶号 = 该小时全部帖子
```

时间分桶是这个形状的最低成本用法——返回单元素 Vec，写入时把时间戳折叠成桶号（桶号定宽 BE，字节序 = 时间序），按小时的 rollup/时间线就是一次前缀扫。同一原语直接覆盖 tokenize 全文检索（切词返回 Vec<String>）、多值字段（tags 拆分）。\`okm-core\` 不内置分词器，切分/分桶逻辑归业务层；func 的契约是**纯函数**——purge 从存储行重新派生待删集合，函数不纯（时钟/随机/外部状态）会在删除时生成与写入时不同的集合，留下悬挂条目。空 `Vec` 也是一种行级过滤（该行不产生任何 entry），但行级条件写成声明上的 `where` 更清楚。

**基础用法三：部分索引（`where` 谓词）。** 只索引被谓词接纳的行——存量里绝大多数行用不到该访问方法时（例如工单 99% 已关闭，只索引未关闭的），条目量与维护成本按谓词后的子集计：

```text
fn is_open(row: &Ticket) -> bool { row.status == 0 }

#[ok_index(open_by_assignee {
    fields(assignee_id, created_at),
    includes(title_len),
    where(is_open),
})]
```

谓词与 `func` 同级，同受纯函数契约约束：purge 从存储行重新派生待删集合，谓词读时钟或外部状态会让删除生成与写入不同的集合，留下悬挂条目。谓词的保证到写入侧为止——条目按索引字段定址，谓词只决定"这次写不写"，所以 put 覆盖写不清理旧条目：行从被接纳入索引变为被拒绝时，旧条目悬挂（与「值变化留下悬挂条目」同一既有契约），清理要用**当时生成它的那一行**去 delete —— 而那条路径已随 delete 退役（ADR-0035 2026-10-23）：覆盖写走 merge / replace，它们先扫存储行的面。读侧没有谁替你校验完备性——部分索引只对"查询条件蕴含谓词"的查询是全的，okm 没有查询优化器，用哪个索引由调用方决定，这条纪律属于建模，不属于机制。

`func(path)` 与下文的 `#[ok_reduce]` 是两个外部扩展机制：func 是**单行派生**（写侧预计算，entry 仍随行生灭），reduce 是**跨行聚合**（可变 value 读-改-写）；集成的复杂形态（FTS/向量/图算法如何落在原语上）见[集成边界](integration/EXTENSION-TYPES.zh-CN.md)。实现细节（编码契约、entry_pairs 覆盖、探针归一化）见 internals 的[函数索引机制](internals/func-index-mechanism.zh-CN.md)。

索引条目物理布局（ADR-0005）：

```
[ ns 2B BE ][ slot 2B BE ][ 数据段（fields）BE ][ 主键前缀（默认取满） ]   value = includes 字段 TLV（无 includes 则为空）
```

判别符 = ns + slot：ns 段划整张表，slot 的段号区分条目类别（主表/索引/reduce/junction，ADR-0016）；声明即注册，无运行时索引簿记。slot 按声明序机械分配（SLOT = `#[ok_index]` 出现的序位），因此**索引声明是 append-only 的**：只能在尾部追加，不能在中途插入或重排——插入会让其后所有索引的 slot 漂移，已落库条目留在旧 slot 位，`scan` 换了前缀后读到空结果（静默错误，不是变慢）。删除声明只是留下无害的 slot 洞（与 ns 编号永不复用是同一纪律，ADR-0002）。另注意：没有索引回填机制，尾部追加的新索引只对之后写入的行生效，存量行不补条目；需要覆盖存量时走迁移双写。

数据段（fields 段）的语义结构是**有序的维度序列**：首位是分组/等值维度，其后是排序维度，它受两重约束——
- **解码约束**：数据段中至多一个变长字段，且必须紧贴主键前缀之前。主键定宽（`KEY_LEN`），从尾部反推切出；其后若还有定宽字段也依次从右往左切；剩下整块就是那个唯一的变长段——它的长度不需要存储，边界由右侧定宽段反推。两个变长段（如 `fields(token, name)`）之间没有边界字节，解码不可能，derive 在编译期拒绝。
- **查询约束（leftmost-prefix）**：前缀扫描按数据段声明序从左往右完整指定。变长段不在末位时前缀语义即破——`fields(city_name, status)` 里裸字节 `"beijing"` 无终结符，会同时命中 `"beijing"` 与 `"beijing2"`。变长段必须排最后，等值/定宽维度排前面。反向的一族——探针比值长、字节由主键尾段凑齐——不构成命中，扫描侧按剩余长度判掉（[索引机制](internals/index-mechanism.zh-CN.md)）；全字段等值语义用 `scan_exact`。

另注意 `includes` 不在数据段内——它在 entry value 里，不参与 key 结构与排序。想把"条目里多带点数据"表达成加 fields 段是建模误区，正确出口是 `includes`（免回表复制）或嵌套条目（存一起）。

**多级文本维度不违反这条约束，换形态即可。** 约束说的是"裸的变长段至多一个且排末位"，不是"一个实体只能有一个文本维度"。需要多个文本维度时，按这个次序选形态：

- **层级维度用代理 id（定宽）。** 省市区三级写成 `fields(province_id, city_id, district_id)`：三个定宽段，合法，且 `[省]`、`[省][市]`、`[省][市][区]` 各是一次前缀扫——分组到哪一级由声明序免费给出。名字走「开放词汇」那三件套（行 payload + `by_name` 面）。收益还不止合法性：改名不动键、键长不膨胀、没有终结符歧义。
- **每个文本维度一条自己的面**（`by_name`、`by_city`…）：一条面里只有那一个变长段，合法；代价是每行多几条 entry，每条面都付永久写成本。
- **文本之后再要定宽排序维度，就用主键尾段。** `fields(name)` + 尾段携带时间/身份，条目 = `[name][尾段]`，得到"(名字, 时间)"的复合序——变长段之后只有主键尾段可以跟随，那正是尾段的第三个职责。
- **两个文本维度都要参与字节序 → 合成一个段。** 用 `func` 让数据段是单个字符串，分隔符放在**值层**（例如 `last\x00first`）：仍然只有一个裸变长段，键层依旧没有终结符，前缀扫 `last\x00` 就是"某姓下的全部人"。分隔符的选择是这条 func 声明的契约（与探针归一化同一条"声明驱动两侧"）；它必须落在值域之外，否则就是经典的转义问题——只是这一层由值侧的函数承担，不由键布局承担。要另一个方向（按名查）就是另一条面（func 扇出）。
- **定宽优先于 VarInt。** 两个 `VarInt` 字段会撞上同一条限制；把宽度写出来（`u32`/`u64` BE）限制即消失——排序段里定宽是常态，VarInt 是省空间的手段。
- **真正的最后手段是自定界帧**：两个裸变长段相邻在结构上不可解，这条路可行但换掉零成本切片与编译期布局锁定（ADR-0005《Update 2026-10-17》）。

代价结算在写侧：每多一条面，每次 put 就多维护一组条目。

### 连接、断开、查询（`Junction`）

```rust
use okm_core::Junction;

let store = okm_core::TestStore::default(); // slatedb-mem；另有 FjallStore / SlatedbStore / RedbStore
let edges: Junction<_, UserToSession> = Junction::new(store);

let user = UserKey { org_id: 7, user_id: 101 };
let s1 = SessionKey { org_id: 7, session_id: 1001 };
let s2 = SessionKey { org_id: 7, session_id: 1002 };

edges.link(&user, &s1);   // 原子双写：正向 + 反向 key
edges.link(&user, &s2);

let sessions = user.get_session(&edges);   // 正向：user → [SessionKey]
assert_eq!(sessions, vec![s1.clone(), s2.clone()]);

edges.unlink(&user, &s1); // 双向同时删除
```

物理 key 布局（正向）：

```
[ ns_a u16 BE ][ slot u16 BE: 0x3 段 ][ A·身份 ][ B·身份 ]   在 A 的集合里
[ ns_b u16 BE ][ slot u16 BE: 0x3 段 ][ B·身份 ][ A·身份 ]   在 B 的集合里
```

本端身份在前（扫描前缀 `[ns][slot][本端身份]` 必须能命中），对端身份是后缀。区分号低位承载方向——只有自反 junction（两端同 ns）需要它。

每条 entry 单向：ns_org 里的回答“这个组织的全部成员”，ns_user 里的回答“这个用户所属的全部组织”。方向由条目所在的 ns 承载——不再有方向 slot（旧的 14/15 已取消）。`nnn` 是 junction 区分号（`#[ok_junction(n)]`），区分同一端点对上的多条 junction。junction 的字段引用**文档类型**，derive 反查其 `Key` 与 `NS_PREFIX`——ns 只在文档上声明一次。见 ADR-0015/ADR-0016 与 [key-layout](docs/internals/key-layout.zh-CN.md) 的 slot 表。

### 反向查询与截断身份

```rust
// 反向：session → users。此方向 A 的身份是截断的（ok_head），
// 返回原始字节，供调用方拿去主表做前缀扫描。
let raws = edges.reverse_raw(&s1);

// 若 A 是全量身份，reverse() 直接 decode 回类型：
// let users: Vec<UserKey> = edges.reverse(&s1);

// PrefixKey 标记前多少字节可信。
for pk in edges.reverse_prefix(&s1) {
    // pk.decoded：解码出的结构体（前缀字段有效）
    // pk.taken：  身份前缀消耗的字节数
}
```

派生宏还会在端点类型上生成查询方法（`user.get_session(&edges)`），方法名取对方字段（`session_id` → `get_session`）。

### 行的运行时用法（`Collection`）

`DocumentEncode` 声明的访问方法在查询侧具名为索引类型。索引声明的派生物在展开点（本文件）生成：`ok_index(by_org ...)` 生成索引类型 `__OkmIndex_User_by_org`（机械拼接，无大小写转换），`use` 别名后即可作泛型参数。`Document::collection` 构建装配点，调用处无需重复 key 类型：

```rust
use okm_core::TestStore;
use __OkmIndex_User_by_org as ByOrg; // 索引类型：ok_index(by_org) 的派生物

let t = <User as Document>::collection(TestStore::default());

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 });

// 任意访问方法上的最左前缀扫描，带回表：
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
for (key, row) in rows {
    // key: 解码的 UserKey，row: payload 存在时为 Some(解码的 User)
}

t.purge(&user); // 删除主键 + 所有已声明的索引 entry（+ 动态段；多值索引逐条清除，无悬挂条目）
```

多值函数索引的声明与查询（倒排索引形态——token 是数据段，主键前缀从尾部反推）。func 返回 `Vec` 是契约而非偷懒：put 收到返回值后立即逐条写入，惰性迭代器没有收益，一次 `collect` 换 trait 面最小：

```rust
fn tokens(row: &Doc) -> Vec<String> {
    row.text.split_ascii_whitespace().map(String::from).collect()
}

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(DocKey)]
#[ok_index(by_token { func(tokens) })]
pub struct Doc {
    pub text: String,
}

use __OkmIndex_Doc_by_token as ByToken;

// 写入：一行 "rust kv" 展开为两条 entry（rust → key，kv → key）
// 查询：token 前缀扫 → 回表，与普通索引无异
let hits = t.scan::<ByToken>(b"rust");
```

等值前缀之外，索引首字段上的比较谓词是一个物理 key 区间（ADR-0020）：`scan_range::<I>(begin, end)` / `scan_range_iter::<I>(..)`——OKM 每种编码都保持字节序 == 值序，`1 < a < 100` 只读 `[1, 100)` 内的行。配方见[查询指南](query-recipes.zh-CN.md)「where 的形态」一节。

### 动态字段：document API（ADR-0012）

一套编码同时服务声明行与外部数据。声明字段照常走热/冷段；其余落进**动态段**（固定面 0x01），以 n-TLV 帧——`[字段编号][类型][长度][字节]`——存储，名字在**字段名词汇表**（固定面 0x02/0x03）里首次出现时分配。`Collection` 上两组动词，区别在“这次调用能看到什么”：`get` / `put` 是**结构体门**（只有行——不载入词汇表、不碰动态段，编译期 schema 这条路），`fetch` / `merge` / `replace` / `purge` 是**整文档门**。

分类只按 payload 声明（热段或冷段），没有别的判据。与 key 字段同名的名字也不特殊：OKM
本质上是 KV 存储，键与值是两件独立的东西，所以那个名字只是值里的一个字段，落进动态段。
身份仍然来自键——任何 map 都移不动它，一个写着 `org_id` 的 map 也不会在 `org_id` 处造出任何行。
这里不需要加检查，也不需要替使用者消除混淆：把键和值混为一谈的人，拿到的是他自己要的东西。

```rust
// 行-映射桥：全部字段（声明 + 动态）lift 到逻辑类型
// （Quant -> F64、VarInt -> u64、Enum -> 变体名、Offset -> i64）。
let fields = t.fetch(&key);                    // Option<BTreeMap<String, DynamicValue>>，None = 文档不存在

// 局部更新：命中声明的名字是一次行 RMW（走 typed 路径，旧面一并清理），
// 未知名字只覆盖动态段里对应的那些名字。
t.merge(&key, &patch);                        // BTreeMap<String, DynamicValue>

// 整体替换：map 就是文档——缺席的声明字段取默认，未点名的动态名被移除。
t.replace(&key, &fields);

// 两槽一次提交；以及带旧行的 typed 删除。
t.purge(&key);                                // -> bool（原来有东西）
// purge 是唯一的删除动词：行与动态段一起走（行不可能活过它的段）
```

- `DynamicValue` 承载开放的值词汇表：`UInt`/`Int`/`F64`/`Str`/`Bytes`/
  `Bool`/`Null`/`Array`/`Obj`——嵌套对象以原生帧递归（类型标签 7），
  共享所在表的词汇表；不依赖 CBOR。
- `Bytes` 是词汇表中的**不透明成员**：存储层不解释任何东西——只有
  tag 与总长度；内容及其含义属于应用。不透明说的是**内容**：帧的
  `[len]` 前缀是 payload 定界（编码层的职责——多字段载荷靠它切分），
  不是对内容的解释；`String` 带同样的头。它是扩展类型的标准逃生舱：
  新编码先以 Bytes 形态落地，未来提升为一等类型是纯增量变更。
  字段名承担 tag 刻意不承担的语义（`embed_v1`、`attrs_cbor`）。
- 未知名字在这里是**正常输入**（外部数据、MQ payload）；「未知字段拒绝」
  纪律只适用于声明路径的 typed 解码器。
- 声明字段走自己的 0x1 面可索引。动态字段可索引，方式是声明一条 path 索引
  （ADR-0029：每个解析出的值一条条目，值类型限有存储形态编码的那些——
  U8/U16/U32/U64/FixedBytes/Str）——已在 schema 驱动的（`DynamicCollection`）
  写路径上落地；typed collection 自己的动态段还没有这套维护。动态**名字**
  永远不能当 key 材料：名字是运行期数据，动态路径只能靠它的声明被寻址，
  不能靠 key。
- 值形态过边界时用它自己，而不是调用方每个方向手写一遍 `match`：
  `DynamicValue` 的 `Serialize`/`Deserialize` 挂在同一个 `schema-serde`
  feature 后面。反序列化走 `deserialize_any`，因此格式必须自描述
  （JSON、CBOR——不是 bincode）；JSON 下有两处避免不了的塌缩，已写进文档：
  `Bytes` 呈数字数组（CBOR 保留字节串），非负 `Int` 读回来成 `UInt`。
  存储编码不变：帧 + 词汇表 id 仍是落盘表示。
- schema 导出（`CollectionSchema`，serde 在 `schema-serde` feature 后）驱动
  嵌入式语言读取器的动态 codec——Python（PyO3）与 Steel binding 在
  `bindings/`，与 Rust derive 字节一致（交叉测试锁定）。版本默认值迁移
  在动态读路径同样生效：字面量 `#[ok_default]` 随 schema 走。

### 动态段上的路径索引（ADR-0029）

访问方法可以命名一条指向无 schema 区的**路径**，而不只是声明字段：

```rust
use okm_dynamic::{AccessMethod, FieldType, Path};

// 嵌套对象字段
let by_author = AccessMethod::path(0x1001, Path::parse("author.name")?, FieldType::Str)?;
// 数组路径：每个元素一条条目（扇出）
let by_tag = AccessMethod::path(0x1002, Path::parse("tags")?, FieldType::Str)?;
// 钉住某个元素：恰好一条
let by_first_href = AccessMethod::path(0x1003, Path::parse("refs[0].href")?, FieldType::Str)?;
```

- **组件**：`Field(name)`——对象键，经该表字段名词汇表入典；`Index(u32)`——
  数组位次（数据：不入典，且不跨变更稳定）；`Key(bytes)`——字面键，不入典。
  前两者与 `Index` 是两个命名空间、两种 walker 操作（map 取值 vs 数组下标），
  不是一个被 niche 的整数。
- **解析**：数组之后的任何组件都会对每个元素扇出（通配**就是"没有
  `Index`"**）；路径止于数组时对其标量叶子扇出；`Index` 钉住恰好一个元素。
  名字缺失、下标越界、路径止于对象，都解析为空。
- **值类型由声明给出**（`FieldType`）：条目段是字节有序的，而路径没有 schema
  字段可查。无符号类型还接受动态读路径产生的加宽形态（写入的 `U32` 读回是
  `U64`）——这正是让"条目字节是存储值的函数"成立的东西：覆写清扫重算出的键
  与写入时构造的完全一致，值变更后不会留下旧条目。
- **维护挂在整文档写路径上**（`merge` / `replace` / `purge`）。主文档路径按
  构造跳过路径声明：主文档上深度为 1 的路径恰恰就是普通索引；一个条目集合只有
  一个维护者。
- 变体与声明类型不符的值不产条目、也不报错——"属性缺失即空结果"同一条规则。
- **查询**就是普通的最左前缀扫描（`scan(slot, prefix)`），值的区间由 ADR-0020
  的范围扫描覆盖。路径的规范编码（自定界、不前置计数）是"路径作为数据"时的
  形态——声明载体与导出的 schema；条目键携带的是 slot，从不携带路径。

### 嵌入文档（`Ref<D, K>` 与 `Refs<D, K>`）

以 **key 引用**嵌入子文档：父文档的字段在 wire 上只携带子文档的 key（定宽、热段）；子文档是独立完整的 document，有自己的 ns/key 和自己的索引。内存形态是 `key` + `Option<value>`：

```rust
#[derive(KeyEncode)]
pub struct AddressKey { pub owner_id: u64, pub kind: u8 }

#[derive(DocumentEncode)]
#[ok_ref(AddressKey)]
#[ok_ns(42)]
pub struct Address {          // 独立完整的 document
    pub city: String,
    pub zip: u32,
}

#[derive(DocumentEncode)]
#[ok_ref(OwnerKey)]
#[ok_ns(41)]
pub struct User {
    pub level: u32,
    pub address: Ref<Address, AddressKey>,  // 无需 attribute
}
```

- **写入 `Some(value)`**（`Ref::own(key, value)`）：父文档的 `put` 同时写子文档 payload 与子文档自己的索引条目，同一 store batch（同一原子边界）。
- **写入 `None`**（`Ref::ref_key(key)`）：引用已存在的子文档——父文档只存 key，不碰子文档。这是共享、多对一的形态（多个 user 指向同一个 address）。
- **读取**：`get` 自动解引用——按存储的 key 取子文档并回填。子文档缺失（引用语义下被独立删除）读回 `value: None`——可见的缺失，不是 panic。查询直接返回嵌套结构体。
- **覆盖**：换 key 会释放旧引用——旧文档指向而新文档不指向的 key 被删除。不 cascade：共享的子文档存活；拥有式级联（`#[ok_embed(own)]`）是可能的后续扩展。
- 无需 attribute：derive 从字段类型识别 `Ref<D, K>` / `List<D, K>`，与 `Reverse<T>` / `Quant<P>` 同一纪律。
- **列表**：`Refs<D, K>` 嵌入多个子文档——wire 是冷段 TLV 帧 `[count][key × n]`；内存是 `keys` + 下标对齐的 `values: Vec<Option<D>>`（悬空引用保持可见）。子 key 必须自带列表内身份（`owner_id + seq`）——OKM 从不往 key 上追加位置序号。旧引用释放覆盖列表缩短：旧列表持有而新列表没有的 key 会被删除。`Refs` 就是上文一对多关系的声明式载体：子文档在独立 ns 范式化存储，父字段持有外键集合。
- **身份分界线**：元素有身份（要独立索引、共享、独立更新）→ `Ref`/`Refs`；纯值元素（`Vec<String>` 字段、动态 `Array` 帧）→ 不适用——标量没有 key，对它做 key 引用是范畴错误。
- map 视图（`to_map`）把嵌入字段 lift 为 `Bytes(子 key)`——wire 事实；子文档的值属于子 collection，不属于这个 map。

嵌入是字段跨文档关联的三种方式之一——`includes` 把值拷贝进索引条目（免回表）、动态帧把值嵌进单个 payload、嵌入引用一个独立文档（共享身份、独立索引、独立生命周期）。

### Payload 版本与字段默认值

payload 头部带版本字节（`#[ok_layout(version = N)]`，默认 1）。解码规则：payload 头部版本比读取方的 schema **新** → 拒绝；**旧** → 接受，且旧 payload 缺失的字段（该版本之后尾部追加的）取默认值：

```rust
#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(UserKey)]
#[ok_layout(version = 2)]            // 字段集变更时递增
pub struct User {
    pub org_id: u32,                 // v1 就有
    #[ok_default(100)]               // 显式默认：v2 之前的 payload 用它
    pub reputation: u32,             // v2 尾部追加
    pub bio_len: u16,                // 无 #[ok_default] → T::default()（0）
}
```

- `#[ok_default(expr)]` 接受任意表达式（字面量、常量、函数调用）；不写则回退 `<T as Default>::default()`。
- 默认值**只作用于解码旧版本 payload**——即字节里缺失的字段。新写入的 payload 总是带全字段（写入方标自己的版本），所以这是版本迁移语义，不是「字段缺省值」。
- 尾部追加是加字段的唯一合法方式：读取方认识但字节里找不到的字段必然在段尾（header 的 `hot_len` 标出热段边界；cold TLV 帧缺席就是不在）。中途插入会改变既有字段的位置 = 布局变更 = version 递增 + 清库重建，绝不静默。
- 惰性迁移：旧记录保持旧格式，升级发生在读取时的内存里。未读到的行永不消耗写带宽。

dynamic codec（Python/Steel 的 schema 驱动编解码）从 `CollectionSchema` 镜像同一规则——字段默认值随 schema 走，动态读取器执行同样的迁移语义。

### Payload 字段类型与包装类型

每个声明字段都落在这几种 wire 形态里。下表是"结构体里的 Rust 类型 → hot/cold 段里的字节"的
映射；derive 按类型名识别包装类型，所以拼写是契约。

| Rust 类型 | wire | 宽度 | 用于 |
|---|---|---|---|
| `u8` `u16` `u32` `u64`、`i8` … `i64` | 定宽 BE | 1/2/4/8 | 数字、id、计数器 |
| `[u8; N]` | 原样 N 字节 | N | 摘要、定长二进制 |
| `String` | UTF-8，TLV 帧 | 0（cold） | 文本、自由标签 |
| `Bytes` | 原始字节，TLV 帧 | 0（cold） | 不透明载荷 |
| `Vocab<N>` | 该字段数据词表里的 N 字节 id（`N` = `u8`…`u128`，默认 `u8`） | 1/2/4/8/16 | 分类值（状态、种类、颜色） |
| `VarInt<T>` | 前缀单调变长整数 | 0（cold），字节序 = 值序 | 需要进索引排序、但通常很小的整数 |
| `Quant<P>`（裸 `Quant` = P3） | 定点 `i64` | 8 | 需要精确存储的小数 |
| `Offset` + `#[ok_offset(base = N)]` | 相对 `base` 的 `u32` 位移 | 4 | 已知窗口内的时间戳或单调值 |
| `Reverse<T>` | 位翻转的 `T` | `T` 的宽度 | 不另开一列就能让索引降序 |
| `Vector<T>` | `[count u32 BE][元素]` | 0（cold） | 同质数值列表（嵌入向量） |
| `Ref<D, K>` / `Refs<D, K>` | 子文档的 key 字节 | key 宽度 / 0 | 进 payload 的文档关系（见「嵌入文档」） |

由这些形态推出三条规则：

- **分类值是 `Vocab<N>`，不是 Rust enum。** 标签↔id 的映射住在 collection 的数据词表里，
  于是动态写入方能引入一个分类、动态读者能说出它。`N` 是 id 宽度，也是唯一可调的旋钮：
  `Vocab<u8>`（默认）每字段 256 个标签、每行一字节；`Vocab<u16>` 65536 个、两字节；依次到
  `u128`。每个字段拥有自己的词表，所以预算按字段算。它没有顺序——需要按语义排序的字段
  另声明一个 rank 列。消费侧的 enum 语义（match 分支、
  穷尽性检查）归消费方：自己包一层 enum，在边界处转换。
- **`Option<T>` 不是字段类型。** 缺失由每个字段自选的哨兵值表达，derive 没有 `Option` 分支
  （见下一节）。
- **`Vocab` 字段在写路径上 intern。** `Collection::put` 与 `save_with_prev_into` 在编码前把
  标签换成 id；`save_into` 只负责编码，因此要求文档已经通过 collection intern 过。一个字段里
  intern 第 256 个不同标签会 panic——收窄值域，或者把这个值建成 `String`。

#### 案例：日志表

高频只追加的日志，宽字段必须自己挣回成本。按上面那张表推出来的声明：

| 字段 | 类型 | 理由 |
|---|---|---|
| `ts` | `u64` | epoch 毫秒：不要包装类型、不要转换，而且普通索引就能排序 |
| `level` | `u8` | 0–5 是**编码**不是标签——映射归消费方，而且它只占一字节 |
| `event` | `Vocab<u16>` | 开放式标签集（`http.request`、`db.timeout`……），动态写入方能扩充、不用重新编译；用 `u16` 是因为 256 个事件类型很快就不够 |
| `module`、`path` | `Vocab<u16>` | 行里带来源模块或访问路径时同理；不带就整条字段省掉 |
| `note` | `String` | 自由文本，永远不做词表——这些值不可枚举 |
| 其余 | 动态段 | 形态随事件类型变化；声明的字段就是每条查询都会过滤或分组的那几个 |

背后的取舍：会被过滤、排序、分组、且类型稳定的字段进 payload；值是"还在增长的集合里的标签"就做成词汇表；形态逐条变化的留在动态段。词表必须是整表共享的，所以它不能住在动态段里（动态值是每行的）。

### 可空字段：哨兵值，而非 `Option<T>`（ADR-0027 审计）

声明的 `Option<T>` 字段**不受支持**——derive 识别的后续讨论已在未实现状态下关闭（PLAN.md Phase 9，2026-09-20 REJECTED：审计过的表没有一个需要它，定宽 presence 字节相比「普通字段 + 哨兵」什么也换不来）。替代惯用法是**从类型的单位元取哨兵值**：

- `String` / `Bytes` 清空 → 空值（长度 0——拼接的单位元）。清空 = 写入单位元，幂等，无需独立的 null 标记。
- 从未更新的时间戳（`updated_at` 类）→ `0`（「未发生」；mudra 的生命周期字段 `opened_at`/`closed_at`/`deleted_at` 全用它，rebinding 会把 `closed_at` 清回 0——哨兵同时承载「重新打开」的状态）。
- 「无父」id → `0`（真实 id 从 1 起）。
- 标志位 → `u8` 0/1，不用 `bool`（wire 上没有 payload `Option`；2..=255 留给未来状态）。

代价是语义过载：`""` 无法区分「显式清空」和「从未设置」。字段真的需要这个第三态时，答案**不是**可空包装而是显式 Enum（`Unset | Cleared | Set(v)`）——多一个 tag 字节，读取侧不再靠猜。动态段的 `DynamicValue::Null` 已经是这个形态：Null 是开放词汇表的一个 variant，不是 `Option` 包装。

经验法则：字段的空态与类型单位元重合 → 用哨兵；「清空」和「从未设置」必须区分 → 声明状态 Enum——不要把状态编码进哨兵里。

### Schema 稳定性测试

用硬编码 hex 锁定物理字节——任何布局漂移都让 CI 失败：

```rust
let e = UserToSession {
    user: Ref::ref_key(UserKey { org_id: 7, user_id: 101 }),
    session: Ref::ref_key(SessionKey { org_id: 7, session_id: 1001 }),
};
let fk = e.a_side_key(); // A 端（正向）entry：住在 User 的 ns
assert_eq!(&fk[..4], &[0, 1, 0x30, 2]); // ns=1（User）、slot 0x3002（junction 段、n=1、dir=0）
assert_eq!(&fk[4..8], &7u32.to_be_bytes()); // 本端身份在前
let rk = e.b_side_key(); // B 端（反向）entry：住在 Session 的 ns
assert_eq!(&rk[..4], &[0, 2, 0x30, 3]); // ns=2（Session）、slot 0x3003（n=1、dir=1）
// ... 完整布局断言见 okm-core/tests/integration.rs
```

## 一对多关系

访问方法不只是排序：**一对多关系也能落成前缀分组**。`[tenant_id][org_id][user_id]` 这样的布局，前缀取一个组织内的全部 user——这正是关系模型里"外键 + 列表"的物理化。

OKM 里它以二级索引的形态出现：索引条目的布局是 `[ns 2B][slot][fields 段][主键前缀]`——`fields(org_id)` 是条目中部的分组/排序段（payload 字段按声明序编码，derive 生成、编译期布局锁定），提供分组前缀；`includes(...)` 是条目 value，复制常用字段免回表。注意这和主键布局无关：分组维度落在索引条目里，可组合、可下线，不存在"后缀无处解码"的问题。也可以直接把用户数据存进列表条目（嵌套式，SQL 里反范式化的"存一起"），存一起效率高，但用户与 org 从此锁定——跳槽要搬数据，与主键复合是同一陷阱。

对应 SQL 的范式化：独立 ns 是范式化的——parent_id、org_name 之类的组织属性只存组织行一份，用户行不带；存一起（反范式化）效率高但耦合，分开（范式化）多一次间接但身份单一。

## 多对多关系：junction

`#[ok_index]` 定义的访问方法是**表内**的——数据源是本行 payload，随 `put`/`delete` 自动同步。跨表关系（一对多、多对多）是两个独立实体之间的事实，payload 索引够不着：一对多由 `Refs` 承载（见「嵌入文档」一节），多对多由 **junction** 承载：

```text
#[derive(JunctionEncode)]
#[ok_junction(1)]
struct OrgUser {
    #[ok_head(tenant_id, org_id)]   // A 端身份截断到 (tenant_id, org_id)
    pub org: Ref<Org, OrgKey>,
    pub user: Ref<User, UserKey>,   // B 端无注解 = 完整 UserKey
}
```

junction 物化为每个端点 ns 各一条单向 key（段 0x3，方向由条目所在的 ns 承载，ADR-0015/0016）：`ns_org` 里的前缀 `[ns_org][0x3nnn][org]` 一次扫描取整组织的成员（列表侧），`ns_user` 里的前缀取某用户所属的全部组织；跳槽 = 增删一条 junction 条目，身份不动。`#[ok_head(...)]` 声明该方向把端点身份截断到哪几个字段（不写 = 完整身份），让端点用更短的前缀、条目 key 长度和分组粒度按方向各自裁剪——端点身份宽度的选取是"主键随方向变化"的表达。`ok_head` 选的是身份字段的子集（结构体声明过的字段），不是自由字节，可解性与索引尾段同理。

junction 与索引机制上同构（都是"指向身份的次级 key 布局"），但角色不能互换：

- **数据源**：索引的数据源是本行 payload，用户永不手写条目；junction 的数据源是两个实体的关系，是业务事实本身，必须显式双写双删。
- **端点**：索引尾段指向自己表的主键；junction 指向另一张集合的实体（双端点、每端 ns 一条单向条目）。

**双向是义务不是选项。** REV 条目只多付一份 key 的存储（LSM 顺序 append，最廉价的写），省掉它换来的却是：反查需求出现时全扫 FWD 段过滤（违反访问方法强制），或事后补边加回填迁移（贵几个量级）。与索引对照更清楚：索引只有一个方向，因为反查走主键 `get` 就行；junction 的两个端点都是次级视角，谁也不持有主键，所以两个方向都要一条。双向还让解绑变 O(1)——FWD/REV 两条 key 的身份都在手上，精确 `delete`，无需先扫后删。真正的克制点不在"要不要 REV"（不二选），在**要不要这条 junction**：没有反查需求且基数小的关系（如配置类一对一），直接放 payload 字段就够，连 junction 都不建；需要时再加，junction 的双写自动同步，无回填。

一句话：**索引 = 一行的派生视图，junction = 一等的关系数据**。`#[ok_index]` 定义时方便（声明即注册，无需手工编号 ns——ADR-0005 的动机正是消灭 per-index 手工编号与洞簿记）是次要红利，不是两者的分界；分界在数据源。

**接口是动词对，不是字段**（ADR-0015 §B，2026-09-20 定案）。`Refs` 字段上的 `#[ok_relation(JunctionType)]`——put 时 diff 字段自动 link/unlink、推广 Refs 的 stale-release——经分析后否决：Refs 成立是因为父行是唯一所有者（结构上单向）；junction 的两端是对等的，单侧字段声明会让对端的 delete 无法清理（残留字段 key 在下次 put 时复活已删的边），且两个写入口（字段 diff + 显式 `link`/`unlink`）会互相打架。命令式成对操作保住调用点即因果记录——代码说发生了什么，永不重建旧状态，写域也从不隐式越过对端的 ns。不要把 junction 建模成文档字段；若单端集合形态的整体重写成为真实需求，出口是显式命令，绝不是 put 路径魔法。

### Refs 还是 Junction：判定，以及答案变化时的迁移

两种载体回答的是不同的所有权问题——用"哪一侧需要集合形态的查询"来选：

- **"这个 user 挂了哪些 session？"**——单向需求。关系是 user 行的属性：字段位 `Refs`，user 行是唯一真相源，session 在自己的 ns 内范式化、不回指。同步形态的变更（导入推送整份列表、表单提交新集合）天然由 put diff 承接——字段即状态，覆盖写 + diff 就是既有机制。
- **"并且：这个 session 被哪些 user 挂了？"**——两个方向都是一等集合查询。关系已超出任何单行的承载范围：user 侧字段无法不经全扫地回答 session 侧的问题，所以事实必须物化到两端 ns——即 `Junction`，以显式事件 link/unlink。

第二个问题后到，是**一次 schema 迁移，不是原地改造**。把 `Refs` 字段提升为 `Junction`： (a) 声明 junction 类型（同一端点对、新的 `#[ok_junction(n)]` 区分号）； (b) 回填——遍历父 collection，对每行的现行 key 集合逐条 `link`（junction 条目没有回填机制，与索引 append-only 同一纪律：新访问方法只看到它存在之后的写入）； (c) 同一次 layout-version 递增中把字段从文档删掉——否则字段和 junction 是争夺同一事实的两个真相源，正是扼杀 `#[ok_relation]` 的那个双写入口冲突。反向迁移（junction 退回 `Refs`）适用于某一端的查询需求消失时——反查需求是事实当初离开行的唯一理由。

被否决的 `#[ok_relation]` 提案实质上是：用第一种形态的真相源（单端字段）承载第二种形态的关系（两端定义）——这个错位正是它站不住的原因。

## 图边：第三种关系载体

Junction 覆盖的多对多绑定在**编译期端点类型**上。知识图谱打破这个绑定：一个图连接多种类型的节点，边**谓词**是运行期数据，平行边是不同的事实，边还携带属性。图边是第三种关系载体（ADR-0017）——独立身份、属性、平行边、开放端点，且声明**就是**这张图。

节点就是普通 document collection。类型集在编译期封闭时，节点的"类型"是它的 collection（ns 即类型标记）；kind 在运行期才到达时，kind 是一个 collection 内部带索引的字段（k10r 案例见下）。两种形态下，节点 kind / 属性过滤都走 collection 自己的声明索引和词汇表——与任何文档 collection 布局零差异。新增的东西都在边这一侧：

```rust
use okm_core::{Graph, GraphEncode, NodeRef, NodeSet};

/// 图的边 collection——声明**就是**这张图（ADR-0017 §4）：一个 ns、边自己的
/// 载荷（就是一份普通文档载荷：热/冷段摆放、两个方向都生成的编解码）、它**显式
/// 声明**的属性面，以及它允许的节点集合。端点不逐个声明——引用自描述
/// （[ns 2B][len][pkey]）——但"可以有哪些集合当节点"要声明。
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(100)]
#[ok_nodes(User, Org)]                         // 成员集：节点**表**，按类型列出
#[ok_index(by_since { fields(since_year) })]   // 面是声明，不自动生成
struct Employment {
    since_year: u16,
    weight: u32,
}

// 由 derive **生成**——不手写，也不需要人跟着改：
//   enum EmploymentNode { User, Org }
//   impl NodeSet for EmploymentNode { const NS_VALUES: &'static [u16] = &[10, 11]; }
// 枚举名单数（图名 + `Node`）：一个类型名命名一个值，一个值指名一个 COLLECTION
// （成员资格 = 该 ns 在不在集合里）；"这些 collection 是这张图的成员"归
// `NodeSet::NS_VALUES`。每个 ns 取自那张表自己的 `#[ok_ns]`——加一个成员就是在边
// 声明里多写一个类型名。

let mut g: Graph<_, Employment> = Graph::new(store);
let attrs = Employment { since_year: 2020, weight: 7 };
g.link(&EdgeHead {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    predicate: "employs".into(),   // 边的类型：一个头部段 + 图自己的词汇表
}, &attrs, 1).unwrap();            // 载荷由声明自己编码（ADR-0017 §3）

g.typed_out("employs", &user1);   // 0x09 面：谓词过滤遍历
use __OkmIndex_Employment_by_since as BySince;   // 声明生成的那条面的类型
g.by_attr_face(<BySince as KvIndex>::SLOT, &2020u16.to_be_bytes());
```

- **端点引用**是 `[ns 2B][len varint][pkey]`——ns 充当类型标记，pkey 宽度以单字节 varint 住在引用自身。任意 collection 的文档免声明参与：同节点 = 同字节（遍历面精确前缀命中），不同 pkey 宽度共存，没有需要声明、维护、防错的注册表。图边让 Ref 纪律退役——引用自带宽度，连同 ns 知识一起。成员集是唯一要声明的东西——以**表的清单**形式（`#[ok_nodes(User, Org)]`），derive 据此生成 `XxxNode` 枚举与 `NodeSet` 实现，每个 ns 取自那张表自己，在写入与解码两处作为一次校验；宽度仍是 wire 数据，成员是声明。
- **面**：一次 `link` 写主表（固定面 0x00，`edge_id` u64）+ 谓词索引（0x06）+ 出入遍历（0x07/0x08）+ 谓词过滤遍历（0x09/0x0A）+ 每条**声明**的 `#[ok_index]` 一条面（计数族 0x1），一个 batch；`unlink` 解码边本体、精确删掉这些条目（声明面按解码出的载荷重新生成，不做前缀清扫）；全动态形态把开放属性写进动态段（固定面 0x01，每条边一条条目）。谓词名走**图自己的谓词词汇表**（固定面 0x04/0x05，先见者得号）——与载荷的字段名词汇表（0x02/0x03）以及任何节点字段的词汇表都分开，所以"列出这张图的谓词"就是扫一次 0x04 前缀，永不靠走索引面反推。
- **句柄不声明任何东西。** `Graph<S, E>` 绑定一个引擎实例与每实例状态（谓词词汇表缓存）；ns 来自声明、成员集来自 `E::Nodes`——与 `Collection<S, K, R>` 同形，后者也只收引擎。
- **平行边**是各自独立的边：每次 link 携带调用方选定的 `edge_id`；`link` 拒绝 live id。一条边的**就地改写**是唯一例外，入口也只有一对：`unlink_into` + `link_into` 把同一个 id 的删与写编进同一批，提交点上那条边从未缺席、第二个 id 也不会诞生——k10r 的 functional 裁决和计数器回写都走这条路（ADR-0010 的 owed item）。已删的 id 不复用。
- **过滤**：先走选择度最高的面，id 在内存收敛——默认不建复合面（`(*)-[谓词]->(:节点 kind)` = 0x09 扫描与节点 kind 面的 pkey 集合求交）。

分界线：`Refs` = 一对多；`Junction` = 多对多、固定端点、无属性；**图边** = 独立身份、属性、平行边、开放端点。

### 谓词：边的类型

关系的名字住在边的**头部**、与端点并列——`EdgeHead { src, dst, predicate }`——而它是这三段里唯一没有编译期声明的东西。这正是 junction 比不上的一环：junction 的类型是它的 Rust struct（`PageTag`），写代码时就定了；边的类型是**数据**。

- **在图自己的词汇表里 intern**：名字 → `u16` id，只追加、先见者得号、永不重排。谓词面与遍历面都以这个 id 为键，所以每一次"按关系过滤"的读都是对 id 的前缀扫，而不是载荷比较。
- **两个方向、一本词表**（`名字 → id` 与 `id → 名字`）："这张图到底有哪些关系"是一次前缀扫，于是谓词集合是可枚举的元数据，而不是从边反推出来的猜测。
- **加一条关系不是一次迁移。** 没有逐谓词的声明、没有代码改动、没有一关系一个 ns：一条边 collection 同时装着 `employs`、`reports_to`、`blocked_by`。id 空间按图划分，所以谓词永远撞不上节点 kind、字段名或另一张图的谓词。
- **谓词是关系，不是属性。** 把 `is_admin` 做成谓词、指向一个 `true` 字面量节点，是典型的失效形态；那个标志是行上的一个字段。谓词回答"这两个东西怎么相关"，"这个节点是什么"该由节点自己的声明回答。
- **节点的 KIND 是字段，不是谓词词汇表。** kind 是载荷里的分类值——`kind: Vocab<u16>`，由 `by_kind` 索引（ADR-0032）——标签集归那个字段、宽度就是声明，别的一点规则都不需要。谓词 = 边在**头部**的类型；kind = 节点在**载荷**里的类型。字段名是 `kind` 而不是 `kind_id`：`Vocab<N>` 下值就是标签、id 只是编码，`_id` 后缀会声称调用方在处理数字。
- **方向是断言的一部分。** `link` 写的是 src → dst；反向关系是另一个谓词（或同一个谓词、端点互换），库从不替你推导。同一对端点上的两条边是两条不同断言——靠各自的谓词与 `edge_id` 区分，这正是平行边的用途。

**为什么一个 `Vocab` 字段替代不了它。** `Vocab<N>` 字段 intern 一个标签，面只有你在 `#[ok_index]` 里声明的那几条；它住在**载荷**里，在载荷自己的帧之后。谓词挪不进去：遍历面以它为键（`[predicate_id][src NodeRef][edge_id]`、`[predicate_id][dst …]`），而端点是**头部**字节、不是载荷字段，任何载荷索引都产不出这些条目；id 还必须待在边本体的可计算偏移上，`unlink` 才能不先解载荷就删掉一条边；而且每条边恰好一个谓词，正是这一点让它能直接进键、不需要存在性检查。两者共用的是底下那套机制——同一个 intern 式标签↔id 协议（只追加、先见者得号、两个面）——所以这是一个原语的两个位置，不是两套机制：词汇表归图的**键**所有，与词汇表归载荷**字段**所有。

### 什么该进图

三条建模判断压在机制之上——开放端点让"什么都塞进图"变得容易，这
三条是拦住它的线：

- **一张图 = 一个信任域 + 语义相关的关系统一收——能合不拆。** 图的
  边界是 ns：一图一条 Edge collection（ADR-0017），**不是**全局隐式
  边池（SurrealDB 的 `RELATE` 形态——每次 `RELATE` 悄悄并入全库一张
  图）。PGQ 的"能合不拆"判据照搬：同信任域、关系语义相关 = 一张
  图；只有语义域彻底分离且从不相交查询才拆两张。图内谓词是运行期
  数据——一条边 collection 承载多种关系类型，即 PGQ "一图多关系"
  的立场，而无需它的 per-pair 声明开销。
- **型与态分置：高 churn 的实例态不进图。** 任务的模板、前置条件、
  引用是图节点（慢变量，值得遍历）；实例行（items、claimed_at、短
  生命周期、精确枚举查询）是普通 document collection。近似遍历不是
  精确枚举的正确载体——"搜一条记忆"不能混进"数一条状态"。应用的
  聚合状态需要精确列出或计数时，那是带 access method 的 collection，
  不是节点。
- **跨图：经 NodeRef 组合，不走图语法。** NodeRef 自描述，同一
  document collection 可以同时参与多张图（PGQ "一张表属于多个图"的
  性质——这里表现为多条 Edge collection 引用同一批节点）。真正分离
  的域互相到达：从一张图收集 NodeRef 集合去探另一张的面，或退回普通
  文档读取——没有跨图 match 语法，也不需要。

#### 案例：标签森林是行 + junction，不是图

mudra 的标签森林正是让人想上图的形状：多根、没有单一根、一个节点同时活在
多棵树里。它由两个载体构成，两个都不是 Edge collection。

```rust
#[derive(DocumentEncode, Clone, PartialEq, Eq, Debug, Default)]
#[ok_ref(TagKey)]
#[ok_ns(1)]
#[ok_index(by_parent { fields(parent_id) })]   // 树：-1 = 根节点
#[ok_index(by_name { func(tag_name) })]        // 裸名寻址
struct Tag { parent_id: i32, name: String, rank: i32, deleted: u8, /* … */ }

#[derive(JunctionEncode, Clone, Debug)]
#[ok_junction(1)]
struct PageTag { page: Ref<Page, PageKey>, tag: Ref<Tag, TagKey> }
```

**树是自引用行。** 一个 tag 带 `parent_id: i32`，`-1` 作根哨兵——"多根"
不需要虚拟 super-root；一次 `by_parent` 上的 scan 就取出某节点的子节点。
反向（"谁是我的子节点"）不需要任何边行，因为它**就是**那条索引。这也是树
与多对多的分界线：父指针的反向是一次 scan，junction 的反向没有单独一行可以
落脚。

两个值得照抄的后果：键保持代理形态（`TagKey { id }`），所以兄弟次序搭不上
键序——它是 `rank` payload 字段，scan 之后在内存里排；也没有 path / closure
表，路径靠向上走 parent 推出来。

**成员关系是 junction。** page↔tag 是端点编译期定型的多对多，于是 `link`
就是原子双写（每个端点的 ns 各落一条），两个方向都是一等 scan（derive
生成的 `get_tag` / `get_page`）。junction 自己不声明 ns：方向由 entry 落在
哪个端点的 ns 表达。

**为什么不是图边。** 这里的边没有属性、没有自己的身份、不允许平行边、端点
在编译期封闭——ADR-0017 里让 Edge 成为第三种载体的四个性质一个都没用上。
一个都用不上的森林就是"行指针 + junction"；硬上第三种载体，正是上一节划线
拦的"什么都是图"那种失效。

顺带一个值得写明的收益：粗粒度的成员位（"收藏"/"星标"）因此没有新地方要
安放——它就是对一条既有轴某个 rank 子节点的一条边，所以不需要自己的
collection。

#### 案例：k10r 的事实图——真正用得上边载体的形状

k10r（记忆服务）把从对话中抽出的内容存成事实：一条事实 = 两个节点 + 一条边。实体是一个节点 collection（ns 36），用户是另一个（ns 42），事实是一条边 collection（ns 37）——三个声明，而标签森林一个都用不上的性质，在这里全部承重。

```rust
/// 节点：一个 collection，kind 是字段。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[ok_ns(36)]
struct EntityKey { scope: u64, node_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(EntityKey)]
#[ok_ns(36)]
#[ok_index(
    by_kind { fields(kind) },             // "某 kind 的全部节点"
    by_name { fields(name) },             // 精确入口
    by_community { fields(community_id) },// 桶读
)]
struct EntityRow {
    kind: Vocab<u16>,       // 分类字段：写入时 intern 它的标签
    literal_num: i64,       // 字面量的整数投影（否则 0）
    community_id: u64,      // 写侧的分桶结果
    name: String,           // 属性，永不充当键材料
    embedding: Vector<f32>, // 不声明面：向量没有键序
}

/// 用户：封闭类型的 collection——人是被登记的，不是被发现的。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[ok_ns(42)]
struct MemberKey { channel_id: u64, member_id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(MemberKey)]
#[ok_ns(42)]
struct MemberRow { display_name: String, role: u8 }  // 没有 kind 字段，没有向量

/// 图 Facts：一份声明**就是**这张图。
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(37)]
#[ok_nodes(EntityRow, MemberRow)]            // 成员集：节点**表**
#[ok_index(
    by_since { fields(since) },                // 时间入口
    by_scope_weight { fields(scope, weight) }, // 渐进读取入口
)]
struct Facts { /* scope、since、confidence、weight、三个计数、两个时钟、8 槽 recent 环 */ }

// 成员集由上面那份清单**生成**：`FactsNode { EntityRow, MemberRow }`——每个节点
// collection 一个变体（成员资格 = 该 ns 在不在集合里），每个 ns 取自那张表自己的
// `#[ok_ns]`（36、42）。枚举名单数（图名 + `Node`）：一个值指名一个 collection，
// "这组 collection 是成员"归 `NodeSet::NS_VALUES`。成员关系是编译期事实，写入与解码
// 各校验一次；引用仍是 wire 数据。人与物住在不同 collection，同在一张图里。
```

**节点 kind 是运行期数据，所以 kind 是字段，不是 collection。** 摄入侧收的是开放类型词汇表：调用方的事实元组点名的 kind 没有任何人声明过——一个术语、一个概念、一个日期。而 collection 是编译期的 `#[ok_ns]` 加一个 derive。于是所有实体住在一个 collection 里，`kind: Vocab<u16>` 就是一个普通声明字段：写入时 intern 它的标签，`by_kind` 给它建索引，读方经该字段自己的词汇表把标签解回来——没有分配代码、没有保留面、没有自己的规则，宽度是唯一的旋钮（两字节 65536 个 kind）。字段名是 `kind` 而不是 `kind_id`：用了 `Vocab<N>`，值**就是**标签、id 只是它的编码，带 `_id` 后缀等于声称调用方在跟数字打交道。判据是：类型集封闭 → 每型一个 collection；kind 集开放 → 一个 collection、kind 作带索引的 `Vocab` 字段。

**域里的人也是节点——都不是抽出来的那种。** k10r 的用户是封闭类型，于是自成一个 collection（ns 42），而他们说明了成员集的用途：这张图不是"我抽到的那些东西的集合"。用户行是管理出来的（显示名、人或 agent），从不是抽出来的，也没有 `embedding`——落点索引只从实体 collection 建，所以查询永远不会落在一个人身上。检索落在知识上；人靠遍历进入，上下文头部就是其中一次 walk。一条画像事实是 `user -[PREFERS]-> iced_americano`：head 在 ns 42，tail 在 ns 36，同一张图。成员在 wire 上给自己命名（`member-{channel}-{member}`），核心把它解析成一条引用——这个字符串从不作为节点名存储，所以改名挪不动任何事实。两个节点 collection 之所以能相遇，是因为引用自带 ns——不把身份行复制进抽取出来的 collection，不做联合节点类型，改名也只动行上那个字段、不动任何边。

**关系就是边的谓词，所以加一条关系就是加一个名字。** 摄入时元组给出关系的名字（`PREFERS`、`RUNS`、`BLOCKED_BY`），图把它 intern 成谓词 id——没有字段、没有枚举、没有声明、没有迁移。于是一条边 collection 装着这段记忆能表达的全部关系，而"这份语料用了哪些关系"是对图的谓词词汇表扫一次，不是把边过一遍。节点那一侧的孪生是 `kind`：同样是标签、同样 intern，但它是节点载荷里的 `Vocab` 字段——它给**节点**定型，谓词给**断言**定型。

**属性是边存在的理由。** 一条事实带着它自己的断言语境：`since`（调用方的时钟）、`confidence`、以及权重系统折成 `weight` 的三个计数与两个访问时钟；关系的开放 props（cause、condition、strength、逐断言的 `functional` 标记）落在边的动态段。junction 没有自己的身份，这些全都没有落脚处。

**近期访问住在一个定长环里，不在计数里。** 计数回答"多少次"、两个时钟回答"多近"，都答不了"这个分布长什么样"——而衰减的账单正是这个分布决定的。行尾因此多了一个 `recent: Vector<u32>`：8 槽、从 2026-01-01 起的秒偏移、升序、`0` 表示未用、写入时右移一格，旧版本行缺这一帧时按声明的默认补满 8 槽。为什么不是 `Offset`、不是 `Bytes`、不设游标、也不做成 wrapper，见「同质列表」一节的案例。

**平行边就是事实。** 同一对节点之间的两次断言是两条真相——来源不同、时钟不同、置信度不同——所以多值是默认，各拿自己的 `edge_id`。一源一值的关系由**每一次断言**自己声明（`functional` prop），因为丢掉一条事实是净损失，多一条边是可以回收的。裁决就是机制那节点名的就地改写：在位者与挑战者互相判定（`since >`，或 `confidence >=`），胜者的行以 `unlink_into` + `link_into` 编进同一批、沿**同一个** id 写成——提交点上那条边从未缺席。计数器回写走同一条路。

**属性落在事实的哪一侧，由它描述谁决定。** 描述**关系**的 props 落边；描述**实体**的 props 落节点行的动态段并且合并——同名属性取较新的值，其余存活。于是一个实体被多条事实共享的属性只有一个家。

**租户不是图的边界。** `scope`（一个用户的语料）领节点 key——`[scope 8B][node_id 8B]`——而边的 key 完全不带 scope，因为边 collection 自己造键。scope 骑在**端点**上：实体的主键带着它，遍历从某个带 scope 的节点出发，就留在同一个 scope 内。边行保留一份冗余的 `scope` 字段，唯一理由是排序面只能按它编码的东西探。用户节点是唯一**不带** scope 的端点（一个 channel 属于一个 scope，所以成员行是 store 全局的），把这种事实留在写入时的那个 scope 里的，正是边行自己那份副本。一个语义域 = 一张图；域**内部**的分区是 key 前缀加端点，永远不是第二张图——按租户分 ns 会把声明、谓词词汇表和代码路径按用户复制一遍。

**不是每一种关系都是一条边。** 一条事实可以在 props 的 `ref` 里点另一条事实的 id——一个 id，或一列——当那条事实不够中心、不值得坐到 head 里。没有任何校验：读的人自己决定跟不跟（`fact_in_scope`，一个 id），悬空链接是读方的 404。可被取用的指针，永远不是写方强塞的子树。

**派生的东西不进图。** `embedding` 是没有声明面的 `Vector<f32>`——向量没有键序——所以搜索是 okm-vector 的 `ExactIndex`，首次使用时从行的帧重建、每次写入即丢弃。面只在有读者时才声明：边上两条，`by_since` 与 `by_scope_weight`，各自报出自己的扫描方。没有扫描方的面，是每次触碰都白付的一次写。

标签森林省下来的性质在这里全部在用：逐断言的身份、断言上的属性、作为不同事实的平行边、以及自带 ns 的端点——一个人与一个物在同一张图里。这就是"伸手去拿第三种载体"与"用错它"之间的全部差别。

## 集合成员关系：按基数选倒排索引或 bloom

没有专门的 `Set` 类型——成员关系由两条应用层路径承担，按集合基数选择：

**低基数——多值函数索引就是倒排索引。** `#[ok_index(by_tag { func(tags) })]` 把一行 fan 成每元素一条 entry（`element -> pkey`），"哪些行含 X" 是一次前缀扫，"行 R 是否含 X" 是一次点查——精确、无假阳性，且倒排面靠 put 语义自动坍缩重复（同元素 + 同 pkey = 同 entry key）。真相源留在行上（func 从行重新生成 entry 集合），纯函数契约保证 delete 正确 unfold。写成本与集合大小成线性——集合几十个元素时付得起，上千就掂量。

**高基数——应用层 bloom filter，存为 `Bytes` 字段。** 每 put 的 fan-out 不可接受时（10 万元素的集合每次 put 写 10 万条 entry），改写成员位图：k 个哈希置位 m 比特，每 put O(1)，以假阳性为代价。存储层看到的只是不透明字节——哈希族、m/n 比例、误报预算全是应用参数；把它们扛进 codec 是把数据质量策略放错层。两条纪律让它不越界：

- **真相源纪律**：bloom 是有损派生物——delete 无法从它 unfold 成员。它必须伴随完整的成员表示（行上的 `Vector<T>`，或集合整体存行外），绝不能单独作为成员关系的唯一记录。
- **查询语义**："可能在集合中"（必要时经完整表示验证）vs 倒排路径的"必然在集合中"。两条路径是互补，不是替代。

## 同质列表：`Vector<T>`

复数建模分类学（ADR-0015 §4）在两个关系载体与异质 `DynamicValue::Array` 之外有第四个成员：**类型化同质列表**。`Vector<f32>` 声明浮点列表，`Vector<String>` 声明字符串列表——元素类型是 schema，长度是数据。

存储是**冷段变长 TLV 帧**（与 String 同落位）：载荷 = `[count u32 BE]` + 元素。同质性是它的价值——定宽标量元素是**裸 V**（每元素零开销；384 维 f32 嵌入向量 1.5 KB，而异质 Array 每元素约 10 字节），动态宽度元素（`String`）逐元素 **LV**。

```rust
#[derive(DocumentEncode)]
#[ok_ref(DocKey)]
pub struct Doc {
    pub id: u64,
    pub embed: Vector<f32>,        // 冷段帧，任意长度
    pub tags: Vector<String>,      // 逐元素 LV
    pub score: u32,
}
```

长度是数据不是 schema：换嵌入模型（384 -> 768 维）只是不同的帧，无需 wire 迁移。应用确实知道预期长度时（合同约定的嵌入维度），`#[ok_len(384)]` 加一个编码期检查——写入边界即承诺执行处；解码不检查（绕过解码器是读取者自己的问题——原始帧字节与未渲染的图片同样不透明）。同一合同导出给动态 reader（`FieldSchema::expect_len`），Python 侧被一致地强制。它是应用合同，绝不是格式约束。

元素是**纯值**——身份分界线（上文多对多一节）把 Vector 挡在关系载体之外：标量没有 key，对它做 key 引用是范畴错误。一对多 → `Refs`；多对多 → `Junction`；表达顺序的值列表 → `Vector`；异质动态列表 → `DynamicValue::Array`（固定面 0x01）。`fetch` 时 Vector 提升为 `DynVal::Array`——动态层没有 Vector 类型，Vector 是存储层格式。多维形状是应用层解释（扁平序列按行优先），wire 上什么都不存。

消费侧（okm-vector）在其上分层构建搜索：精确连续扫描（小规模路径，也是召回的参照系）、量化码上的内存 HNSW（int8 余弦或乘积量化码），以及按分片组织的句柄——每分片一份快照存在固定面 0x0B 下，是缓存、随时可丢。图给出的是码级距离，回存量的 f32 重打分由调用方做。存储保持字节不透明：语料仍然只是那些帧字节。

#### 案例：定长访问环住在 `Vector<u32>` 里，不住在计数里

k10r 的事实行要回答一个计数回答不了的问题：这 50 次读是挤在一周里，还是摊在两年里？权重系统的衰减正是按这个分布计账的，而 `read_count` 加「上次访问」只留下两个数字。它给出的形状是一行 8 槽的定长环：

```rust
pub struct Facts {
    // …热段：三个计数 + 两个访问时钟…
    #[ok_default(::okm_core::Vector::new(vec![0u32; RECENT_SLOTS]))]
    #[ok_len(8)]
    pub recent: Vector<u32>,   // 冷段帧：最近 8 次访问，最旧在前
}
```

每槽是从 `1_767_225_600`（2026-01-01T00:00:00Z）起的**秒**偏移；升序、`0` = 未用（那个瞬间永不是一次真实访问，天然可当哨兵）；写入时整体右移一格，队尾就是上次访问。窗口只覆盖最近 8 次，更早的量仍由计数概括——这是行宽与保真之间固定价码的交换，不是遗漏。

四条否定，每一条都是更省事的错答案：

- **不用 `Offset`。** 它是标量字段的拼写（`#[ok_offset(base)]` 把 i64 存成 u32 差值），而 `Vector` 的元素只认 `f32/u32/i32/u64/i64/String`——base 只能留在消费端的常量里，okm 看到的是普通 `u32`。
- **不用 `Bytes`。** `Vector<u32>` 已经给了定宽元素裸 V 的零开销布局、`FieldType::Vector` 的列类型，以及 `#[ok_len]` / `expect_len` 这条写边界契约；用不透明字节，这三样都要在应用层重写一遍，而且动态 reader 与 arrow bridge 都读不懂它。
- **不设游标。** 每次触碰 `weight` 都会变，整行本来就要重编码，8 个槽无论怎么摆都要写一遍——右移的成本是零；游标换来的只有读时必须旋转视图、`0` 哨兵位置不再固定。
- **不做成 wrapper。** 环没有新的 wire 形态（就是 `[count][u32 × 8]`），K、起点、秒级全是应用参数——抬进 codec 等于让每个消费者继承一个它没要求的时钟语义，与 bloom filter 那条纪律同构。

加这个字段不需要清库：尾部追加是加字段的唯一合法方式，旧行缺这个冷段帧就按 `#[ok_default]` 补 8 个 0，`#[ok_len(8)]` 只约束新写的行。唯一要记住的坑在应用侧：`Default` 必须手写，派生的 `Default` 会给一个空 `Vector`，第一次编码就在写边界 panic。

它也不是「让库替我数」的候选：reduce 是跨行 GROUP-BY（累加器的单位是「组里有几行」），subscribe 只把写当事件发出去、从不改字段。「每次访问 +1」只有调用方读-改-写，单写者纪律保证它不被交错。

## 字段太大，放进副表

一列的体量可以比这一行其余部分大几个数量级——一个 blob、一份渲染好的文档、一个 embedding。留在 payload 里，它的代价是：读这一行必付、写这一行的**任何**字段都要把它重编一遍。答案不是往行 codec 里再加一个存储位置（ADR-0036），而是一张键类型为父表键类型的副表。

```rust
#[derive(KeyEncode, Clone, PartialEq, Debug)]
pub struct PostKey { pub id: u64 }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]
#[ok_ns(1)]
#[ok_name("posts")]
pub struct Post { /* 只放热列：author_id、ts、title */ }

#[derive(DocumentEncode, Clone, PartialEq, Debug)]
#[ok_ref(PostKey)]          // 同一个键类型：key payload 逐字节相同
#[ok_ns(44)]                // 自己占一个 ns——这就是全部代价
#[ok_name("post_payloads")]
pub struct PostPayload { pub mime: String, pub body: Bytes }
```

| 要做的事 | 做法 |
|---|---|
| 读一列 | `payloads.get(&key)`——一次点读，行根本不解码 |
| 写一列 | `payloads.put(&key, &payload)`——一次行写，热行根本不重编 |
| 成对写且原子 | 两张 collection 各 `save_with_prev_into` 进同一个 `Batch`，再一次 `commit_batch`（ADR-0003、ADR-0031） |
| 整文档 | 两次读，按需——拆分换来的就是这个 |
| 「有没有 payload」 | 副表的这一行是否在场；不需要 `Option` 字段，也不需要哨兵 |

删除归调用方：这个模型里没有级联（junction 条目是同一个形状），所以这一对由调用方一起删。写的那一对是一次提交；删的那一对在可进批的 `purge_into` 落地之前是两次（见 PLAN）。

拆的时机：这一列很大，**并且**它单独被访问——下载、缩略图、一批重算 embedding。不拆的时机：每次访问都连行一起读，那样只是白付一次读，payload 才是更便宜的位置。

否决：行 codec 内的专用标注（`#[ok_offrow]`，把值抽到同一个行键下的独立面，再配一个 per-document 的 tag enum 当字段级选择子，ADR-0036）。它的代价是第五个存储位置穿过 `FieldDesc`、`FieldSchema`、`split_names`、四个整文档动词与 derive 的各条 walk，而且禁止对这一列建索引或 reduce 分组——换来的只是整文档读最多省一次 round trip。

## 跨行预聚合

索引的每个条目都是**随行生灭**的 append-only 派生视图——purge 用同一个函数重新生成待删集合，永不悬挂。有一类需求天然落在这个纪律之外：按作者统计发文数、按小时的 rollup、精确计数器。它们是**跨行**的——func 的签名 `fn(&Row)` 只看得见一行，答案落在 entry value 的**读-改-写**上，即第四个原语（可变聚合 entry）。

okm-core 对它的立场是两层拆分：核心不内置任何聚合语义（没有内建计数器类型，不解决分布式累加协议），但机械部分由辅助设施提供，声明方式与索引同款：

```text
#[ok_reduce(AuthorStats { group(author_id) })]
```

`group(...)` 从行字段取分组段（entry = `[ns][slot][group 段]`，slot 是 reduce 段的槽位，与索引的槽位计数器各走各的，ADR-0016）；`AuthorStats` 是用户类型，实现 `ReduceLogic`——`Acc`（累计器类型，实现 `ReduceCodec` 定宽 BE 编码）+ `fold(acc, &row)`（put 时）+ `unfold(acc, &row)`（delete 时）。写入路径自动读-改-写：读到当前 acc，fold/unfold，写回。读侧 `reduce_get` 取单组、`scan_reduces` 扫全部组。机制细节（账本不变量、覆盖写的 unfold 补偿、写路径时序）见 internals 的[reduce 机制](internals/reduce-mechanism.zh-CN.md)。

两条使用纪律：

- **可逆性是契约**：`unfold(fold(a,x)) = a` 必须精确成立，count/sum 可以，median/distinct 不可以——不可逆聚合去旁边的 OLAP 系统。复合 acc（count+sum 求均值）直接实现 `ReduceCodec`，okm-core 只负责存取字节。
- **单写者边界**：hook 是读-改-写，单写者引擎下安全；多写者竞态与分布式累加协议不在此模型内（见集成文档）。

零值回收刻意不做：组空了 entry 仍在（acc 回到单位元），省掉墓碑逻辑；调用方按需跳过单位元组。

## 读一个 reduce

写路径维护账本，读侧是两个函数，两者都收**类型 marker**、不收 slot 号——derive 为每条
`#[ok_reduce]` 声明按声明序生成一个 marker，它带着该 reduce 的 slot、分组字段与分组编码器：

```text
reduce_get::<Store, __OkmReduce_AuthorStats_0>(store, ns_prefix, &key, &row) -> Option<Acc>
scan_reduces::<Store, __OkmReduce_AuthorStats_0>(store, ns_prefix)           -> Vec<(group 段, Acc)>
```

- **entry 键是 `[ns][slot][group 段]`**——group 就是身份，没有 key 尾段。末尾那两个参数
  （`&key`、`&row`）只干一件事：编码 group 段的字节（分组字段名按声明序、各自从自己的来源
  编码——双源规则）。只是要读的时候，传一个带着这些分组字段的值即可。
- **`None` 是一个真实答案**：该组从未被创建。零状态不做垃圾回收——被清空的组把 entry
  留在累计器的恒等值上——所以「不存在」与「空」怎么区分是调用方的策略，不是核心的。
- **slot 按段分配、只增不复用**：reduce 的 slot 来自 reduce 段（`0x2`），与索引的槽位计数器
  无关、永不复用——所以一条声明的 slot 在这张表的生命周期内是稳定的，同一张表上的两个
  reduce 靠它区分。
- **`scan_reduces` 返回 group 段，不解成 key**（ADR-0024）：字段宽度在调用方手上，它能自己解；
  读侧助手刻意不返还类型化 key。

## 两种读-改-写：reduce 与 upsert_with

reduce 之外，写路径还有命令式的一半——`Collection::upsert_with(key, f)`：读旧行、闭包算新值、走正常 put。两种 RMW 同一底层形态（read → compute → write），分工按「逻辑谁知道」切：

- **reduce（声明式）**：`#[ok_reduce(Logic { group(f) })]` 在编译期定死——哪些行进哪个组、fold/unfold 怎么算，都是类型声明的一部分，框架驱动。适合与行结构同步演化的聚合（计数、求和）。
- **upsert_with（命令式）**：`f(Option<R>) -> R` 在运行时收到旧行，任意逻辑。适合调用方才知道的更新（余额加减、条件修补）。`None` = key 不存在（插入路径）。

两者共用同一条写路径（put），所以索引维护、reduce 折叠、事件发射全部照常触发，无特例。也共用同一个正确性边界：**单写者**。OKM 是进程内库、每个集合一个持有者，动词取 `&self`，所以这条边界由类型承担：epoch 计数器是一次写唯一触碰的可变状态，它是 `Cell`，于是集合是 `!Sync`，第二个持有者在 get→f→put 进行中无从插手。无需 CAS，这也是 reduce 恰好一次的同一性质。多写者未来（乐观 CAS）是不同机制，不在此模型内。覆盖写的 unfold 补偿由 put 内部完成，upsert_with 不另平账（见 internals 的 reduce 机制）。

## 写路径事件：两个机制（reduce、subscribe）

reduce 回答"累计后的状态长什么样"；另一类消费者要的是写本身作为事件——副作用、搜索索引同步、下游通知。两个机制的分界不是"谁先跑"，而是**消费者代码放在哪里**：reduce 把消费者放进写内，fold 就是写的一部分；subscribe 把消费者放到写外，注解处没有 handler，只有一次 send。判据仍是失败语义：这个消费者丢了一个事件会怎样？账本错 = reduce（写内，构造上恰好一次）；能事后追平 = subscribe（写后，尽力而为）；无所谓 = 你自己的代码（写路径边缘的旁路副作用，okm 不提供机制，也没有声明面）。**机制只有两个**；第三类不配名字，因为它没有可声明、可装配的东西。

- **reduce**（`#[ok_reduce]`）：运行在写路径内部，构造上恰好一次——fold 就是写的一部分。reduce 永远不消费订阅。
- **写路径边缘的旁路副作用**（不是机制）：普通应用代码，okm 无机制、无声明面。它同步跑在写序列里，但**至多一次**、无事务边界、无 undo——「已经通知过」「已经写了另一张表」撤不回来，失败既不回滚写入也不赢得重试；任何必须恰好一次的东西都不要挂在它上面。它与 reduce 的不对称（可逆 vs 至多一次，同一个事件源上）是刻意的设计（ADR-0008 记录在案），不要试图把它做成第二个声明面。

  这不是声明，是写路径边缘的普通代码——行与它的派生条目落定之后顺手执行：

  ```rust
  // 调用方自己的 put 封装：行写入保持恰好一次，通知顺带发出、尽力而为、绝不重试
  fn save_config(store: &mut impl VirtualStorage, cfg: &Config) -> Result<()> {
      let t = Collection::new(store.shared_handle());
      t.put(&cfg.key(), cfg)?;                       // 写：恰好一次
      let _ = METRICS_GAUGE.set(cfg.name(), cfg.ttl_secs()); // 旁路副作用：至多一次
      let _ = notifier.send(cfg.name());             // 旁路副作用：发后即忘
      Ok(())
  }
  ```

  旁路副作用与 subscriber 的分界：前者跑在**调用方的写序列内**（它的耗时记在写的延迟上，
  它的 panic 会传出来）；后者跑在**写返回之后**。如果副作用必须在任何负载下观察到每一次
  写，那它得是带持久供给的 sink 消费者，而不是 put 旁边顺手的那几行。

- **subscribe**（`#[ok_subscribe]`）：best-effort 投递，无保证。注解声明"该行类型的写路径
  事件进入订阅"；注解处没有 handler——derive 只发一次 send，订阅者（subscriber）在装配期
  注册 sink：

```rust
// 声明：bare——该行类型的写事件进入订阅；
// 事件 enum（variant = 行类型名，默认 `RowEvent`）由 build.rs 收集生成
#[derive(DocumentEncode, Clone, Debug)]
#[ok_ref(AccountKey)]
#[ok_subscribe]                        // Account 现在会发出 Put/Delete 事件
#[ok_ns(21)]
pub struct Account {
    pub balance: u64,
}

// 消费：装配期一次性注册 sink
// （tokio mpsc、crossbeam、no-op 均可——传输不是模型的事）
okm_subscribe::SINK_ROWEVENT.register(move |ev: okm_subscribe::RowEvent| {
    let okm_subscribe::RowEvent::Account(e) = ev else { return false };
    index_sync(e.op, &e.key, &e.document);   // 例如搜索索引同步：
    e.epoch == last_epoch(e.key)             //   会丢、可重同步
});
```

消费者收到的是：`op`（Put/Delete）、该行的 `epoch`（发出集合的单调写计数器——精确的
同表顺序边界）、key、完整行快照。`register` 无返回值，投递是尽力而为：队列满会丢、
没有 sink 会丢、写路径毫无察觉。这就是契约——容忍不了丢事件的消费者属于写内（reduce），
不在这里。

事件流还有两条与消费类别无关的性质：事件带单调递增的**写批次 epoch**（发出表的写计数器——同表事件带精确的批次边界，消费者组合子可以恰好折叠到边界为止，glitch-free，而非靠去抖启发式）；流只在进程内有意义——不持久化，重启归零，不提供跨表顺序（独立 put 之间不存在原子性的"两者都已更新"时刻，多表 fan-in 结构上就是最终一致）。

三条使用纪律：

- **旁路副作用的失败不回滚写入。** 它是至多一次的副作用——失败了既不重试也不撤销，写路径不为它买单；反过来，凡是失败需要补偿的逻辑都不该写成写路径边的旁路副作用。
- **正确性永不放上 subscribe。** 队列满会丢、没有 sink 会静默丢；必须恰好一次发生的事（如 reduce）属于写内。subscribe 的位置是容忍丢失、事后可对账的消费者。
- **纯度约束同样适用于事件载荷**：行快照按原样随事件走；消费时再去派生额外上下文，等于重读一个事件已不再保证描述的存储。

传输是组装点决策（`SinkCell::register` 接受任意 sink——tokio mpsc、crossbeam 队列、no-op）；core 保持同步、不认识任何 executor。对 receiver 的组合子（map/filter/merge/折叠到 epoch 边界）属于 stream 层的职责，不属于模型层。

## 访问方法强制

查询只走 `get`（主键点查）或 `scan::<I>`（具名访问方法）。没有通用全表扫描的查询面。

仅有的裸扫描是 `scan_rows_raw` / `scan_keys` / `scan_document_keys`——它们是 snapshot 导出器与 Arrow bridge 的维护面，不是应用查询路径。应用代码伸手用它们，说明缺一条访问方法声明，不是捷径。

枚举有两种模式，因为文档是**两个槽**（ADR-0035）：`scan_keys` 枚举**行**（一次前缀扫——只有段的文档不在这里），`scan_document_keys` 枚举**文档**（行槽与段槽在内存里归并，一次没有声明名的 `merge` 也因此在列）。物化不归任何一种模式管——`fetch` 是那道门，所以 key 变成文档就是一次朴素的 `map`；而归并不能做成适配器，适配器无法吐出一个从未被扫到的东西。

## 键的命名：朴素前缀 + 访问方法组合

键的命名就是三段：`[ns][pkey...][order]`。排序字段放 key 尾（u64 大端时间戳 = 时间序的字节序，天然时间线）：同一 pkey 下的条目物理相邻且按序排列，前缀扫描直接就是时间线/区间读——恢复会话、提取最近 N 条，都是一次 `scan`。主排序字段进 key 是物理决策；其余排序维度不进 key，放行 payload、经 `#[ok_index]` 暴露。

不建议花哨的前缀方案（路径式、语义分段式前缀）——前缀越长越难管理，每个前缀都是一条要维护的编码约定。宁可多声明访问方法：每个 `#[ok_index]` 是一条独立的、可组合、可扩展的读取路径，声明即注册，删一行即下线。

**查询结果是复数的时候，才需要考虑排序键。** 排序键（`[order]` 段）的价值是把"一组相关条目"聚成物理相邻且有序的一段，一次前缀扫描整组取回；单条结果（点查）用 `get`，key 里攒排序段没有意义。

复合键里的排序段也要具体分析。先看排序键的物理性质：**time 这类排序键选择性极高，没有分组能力**——排序键取值几乎每条不同，落到它后面的字段已经"每条一组"，组内排序无从谈起。所以高选择性排序键之后再往 key 里加字段没有意义，它永远只能贡献扫描成本，贡献不了读取路径。

反模式：`[session_id][user_id][time]`（会话消息）。

- user_id 排在 time 前面，条目按"先按用户分组、组内再按时间"排列——一般不存在"在一个会话内按用户分组"的查询，user_id 这一段挤不出任何读取路径。
- 代价是确定的：即便取全部消息（最普通的会话恢复），返回顺序也不是时间序，读侧还要重新排序；提取最近 N 条更是要扫全组再排。
- 真有零星的按用户过滤需求，直接全扫过滤即可，不值得为它改键布局。

正确形态是 `[ns][session_id][time]`：pkey 后只带一个排序键，恢复会话、翻页、区间读，一次前缀扫描直达。

一般化：**`[pkey][排序字段][剩余标识]` 尾缀剩余标识也是反模式。** 把"还要按 X 查/分组"的维度塞进 key 尾，本质上是在 key 里手工维护一条访问方法。手工维护的键前缀在特定场景确实能换来更高性能，但：

- **依赖特定场景**：键布局刻死一种读取路径。业务复杂度上去之后，大部分的表都会需要多种访问方法，只针对其中一种优化意义不大。
- **依赖业务逻辑本身**：优化是否成立，取决于业务恰好支持这种读取模式，不是建模技巧能决定的。
- **成本收益不成比例**：开发成本高，性能收益也不大（量化分析见下节「设计约束对性能的影响」）。
- **后缀无处解码**：key 尾的自定义段需要配套的解码方法，这是 OKM 明确不支持的——`KeyEncode` 派生只解声明过的 key 全长（`KEY_LEN` 编译期锁死），`PrefixKey` 只解截断的身份前缀（前缀之外按契约是垃圾字节）。自己手写偏移算术去解后缀，复杂且易错，恰恰丢掉了 OKM 的意义：编译期布局锁定的零成本编解码。

这些维度一律划到访问方法/二级索引：`#[ok_index]` 声明即注册，可组合、可扩展、随业务增长追加，无需改动主键布局。

## 开放词汇：名字 → 代理 id

键段是定宽字节身份——`KeyEncode` 在编译期拒绝 `String`。一个从 schema 之外到来的名字（用户 id、租户标识、实体种类、路由值）没有定宽，基数是数据规模：既不能声明成 key 字段，也不可能为每个值准备一个编译期常量。三件套把它变成键：

```text
行      [ns][id u32]                     → name + payload
索引    by_name { fields(name) }         → name → id（字符串只住在这里）
reduce  HighWater(id) { group(global) }  → id 发号（点读水位、+1）
```

- **行以代理 id 为主键、名字作 payload 字段**。名字是唯一变长的部分，永不进 key。
- **`by_name` 索引是查名字的方向**（`name → id`）。索引文本段没有长度前缀也没有分隔符（ADR-0005），所以裸前缀扫描会命中更长的名字（"add" 命中 "add_to_cart"）——前缀语义如此，不是假命中。等值入口用 `scan_exact::<I>`：探针吃尽字段段即精确命中，不必回表核验；探针被主键尾段字节满足的那一族（见「索引机制」）由扫描侧按长度判掉。
- **`HighWater(id)` reduce 发号**：读水位、+1、写回。水位永不下降（unfold 是 no-op），所以撤回行不会让下一个 id 变小——账本唯一一处故意不等于行集合的地方，也正是这个 preset 能当发号器的原因。

两条纪律：

- **id 只增不复用**。复用代理 id = 把旧行读成新实体——与命名空间同一条规则。
- **发号器是单写者**。水位的读-改-写与本文档其他读-改-写受同一条单写者边界保护。

反向（`id → name`）是主表的一次 `get`——这正是词汇表胜过哈希的地方：把一个名字哈希进定宽段会碰撞（碰撞 = 静默寻址到别的实体的行），而且哈希不可反查，运维无法用人话列出这份词汇表。

封闭 vs 开放：**封闭**词汇——系统自己声明的那些种类——配真实 ns（`#[ok_ns]`）；**开放**词汇——作为数据到来的名字——配代理 id + 文本索引。okm 自己的命名空间词汇表与字段名词汇表都是这个形态。

## 两种尾段：索引尾可解，主键尾不可解

"key 尾部携带数据"有两种形态，可解性完全不同，容易混淆：

```text
索引条目  [ns 2B][slot][fields 段][主键前缀]   ← 尾段永远可解
主表条目  [ns][pkey...][自定义尾段?]       ← 自定义尾段无处解码
```

**索引尾段可解**：索引条目 key 的尾段就是主键编码——`#[ok_ref]` 声明过的结构体，`KeyEncode` 派生知道它的精确布局，从条目 key 末尾切出来即解回主键（`PrefixKey`，ADR-0005 的契约："the tail is always decodable from the last bytes of the entry key"）。`fields(...)` 段同理：payload 字段按声明序编码，编解码全是 derive 生成的编译期锁定代码。

**主键自定义尾段不可解**：主键 key 的布局由用户声明的 key 结构体决定，`decode` 只解声明的全长。key 尾塞一个结构体里不存在的"剩余标识"，任何生成代码都不知道它的偏移和宽度——`PrefixKey` 的契约还明确截断前缀之外是垃圾字节。

根源一句话：**尾段可解的前提是布局进了声明系统**。索引尾段（主键编码、`fields` 段）在声明系统内，所以可解；主键尾的"剩余标识"在声明系统外，所以无处解码——也不要塞，需求归 `fields(...)`。

术语约定：**"访问方法"（access method）是建模层的词**，指一条已声明的读取路径——建模指南用它；**"二级索引"（secondary index）是机制层的词**，指 `#[ok_index]` 生成的索引条目布局——ADR 与 README 用它。`#[ok_index]` 是两者的挂载点。本文档面向建模，正文一律用"访问方法"，提及机制布局时才落到"二级索引"。

## 设计约束对性能的影响

上述约束不是免费的，但代价有界，且换来的结构收益大于成本：

**key 里直接取 vs 访问方法取。** 剩余标识烧进 key，扫描时随条目免费带回（key 尾字节直接在手）；走访问方法，这个维度要经一次间接操作取回——`scan` 回表每条多一次主表点读加一次 `decode_payload` 解析，`scan_covered` 免点读，代价改解条目 value 里的 includes 段（一个 `Covered` 结构体）。单条仍然多付一次解析。

**单次成本不高，摊销了。** 那一次间接是常规 LSM 点查/一次 TLV 解析，不在热路径的量级上；换来的是键布局不用为每个"还要按 X 查"的维度变形，业务增长时加 `#[ok_index]` 而不是重新设计 key。

**批量场景反而更优（前提：leading 字段被等值前缀固定）。** 回表看似逐条随机点查，实际两条流本身有序：索引条目按 `[索引字段][主键前缀]` 聚簇，主表按 `[主键]` 聚簇——leading 字段固定时尾段就是主键字节本身，回表天然退化为双指针归并（类似 merge join），顺序 I/O，远低于逐条随机读的开销。**跨 leading 字段给区间则不再成立**：尾段改按被区间的维度排，每个命中都是一次独立点读，局部性 ≈ 区间内不同 leading 值的个数——需要区间读的维度请进主键 `[order]` 段（区间读就是主表自身顺序扫），或走 `scan_range_iter` 提前停、只对幸存者回表。key 布局手工优化在批量场景并没有额外牌可打。

结论：访问方法的间接成本是一次可摊销的解析加一次可归并的有序读，用可控、有界的运行时开销，换掉键布局的不可控复杂度——这笔交换在业务复杂度上去之后只赚不赔。

## 覆盖索引克制

`includes(...)` 把 payload 字段复制进索引条目，`scan_covered` 因此免回表，并直接返回解码后的覆盖字段（`I::Covered`，每个带 includes 的声明生成一个结构体）。这对高扇出读路径是真实收益——但每次 put/delete 都要付永久的写成本。

纪律：

- **默认只用 `fields`。** 先用排序字段建模。
- 测得回表路径是热点之后才加 `includes`。它是对已证实热点的读路径优化，不是建模时的默认动作。

## 主键复合的边界

主键可以复合（见四层建模法：`[ns][pkey...][order]`），但复合的成分必须是身份结构的一部分，而身份结构由业务回答，不由建模技巧决定。

典型是 SaaS 多租户：tenant → org → user 三级。tenant_id 和 org_id 绑定（org 属于唯一 tenant），user_id 则不与 org 绑定——用户可以跳槽，换 org 而身份不变。落到具体布局（方括号 = key 段，花括号 = row 字段；同一字段既在 key 又在 row 时两处都写）：

```text
组织（含层级，parent_id 表达树）
  [ns:org][tenant_id][org_id]{name}{parent_id}

用户
  [ns:user][user_id]{name}...{org_id}
```

- 组织主键是 `[tenant_id][org_id]` 复合：tenant_id 画数据归属的物理边界，org_id 是 tenant 内的身份，两者都是身份结构。
- 树形结构不跨 tenant：parent_id 引用的父组织与子组织同属一个 tenant，因此组织表内的 `{parent_id}` 就够表达层级，不需要单独的邻接表实体——逐层展开（BFS/DFS）在 `[ns:org][tenant_id]` 前缀内就是按 parent_id 逐层 `get`，一条访问方法即可覆盖。
- 用户主键只有 `[user_id]`。org_id 是 row 字段——把 `(org_id, user_id)` 拼进主键，跳槽即换 key，同一物理人被拆成多条身份。

org 维度的读取走索引：`#[ok_index(by_org { fields(org_id, ...) })]` 用一次前缀扫描回答按 org 的读取，身份保持单一。

"用户产生的数据属于公司还是属于个人"这类归属问题同理——它们是业务问题，答案变了（数据迁移、所有权变更）身份不该变。这就是行模型（ADR-0006）的 payload-not-identity 规则，落成建模纪律。

## 决策在声明期落定

本文档的全部纪律出自同一条立场：结构性决策在声明期一次性落定，运行期只执行声明。这条立场就是本文档反复回到"声明完就别再动"的原因。

- **运行期没有解释器。** okm 没有查询规划器：一次读取就是一次前缀扫描，一个问题能否被回答由声明期的数据段顺序决定（见上文最左前缀约束），不在查询时选路。说不出访问方法的维度，就是全表扫描。
- **一份声明展开成两份同步产物。** derive 一层产直接操作字节的代码——运行期跑的就是它，一层产常量表（`PAYLOAD_FIELDS`、`FIELDS`、`HOT_WIDTH`、`NS_PREFIX`、每个访问方法的 `SLOT`）。所有非热路径的消费者（schema 导出、动态读取器、Arrow/Parquet 桥、快照工具）读这些常量表，而不是重新推导声明：声明即注册，没有运行时注册表（见[架构总览](ARCHITECTURE.zh-CN.md)）。
- **位置即身份。** 访问方法的 slot 就是它在声明里的序位，字段的词汇表编号就是它首次出现的序次。两者都只增不复用（见上文）：布局里没有"只是个编号"、可以随手重排的东西。
- **非法形态在编译期不可表示。** 索引段里两个变长字段之间没有边界字节，derive 直接拒绝该声明；junction 的端点身份必须是声明序前缀（宏生成的编译期检查）；变长字段不在末位就破坏前缀语义。三条都是编译期拒绝，不是运行期校验：非法模型编译不过，不会在以后某个时刻才失败。
- **地址空间是预算。** ns 是 2 字节身份——一类实体一个 ns，永不复用；slot 空间是 4 位段（条目类别）加 12 位计数，段位按类别预留（ADR-0016）；词汇表编号只增不减。分配它们是声明期的职责——多实例共用一个引擎时，ns 分配是部署期决策。
- **布局变更 = 全量重建，不是迁移。** 索引条目是派生状态：不进导出与快照，导入时由 `put` 从行确定性重建。改 key 布局或段序，就是重新声明加一次清库重建，从来不是原地迁移。运行期可演进的是 payload 形状（尾部追加字段、缺席字段按默认值补齐，见上文版本规则），不是布局。
- **声明有两个时机档，运行期那一档只覆盖数据形状。** 类型路径（编译期 derive）与 schema 路径（运行期 `CollectionSchema`，Python 与 Steel 绑定据此建集合，全程不出现 Rust 类型）按契约产出同一份字节。两者之差只在 payload 形状；key 布局、段位与 slot 仍由声明决定。

## 演进

新的建模原则以小节追加到本文档；机制决策进 ADR 并引用，不重复。
