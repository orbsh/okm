# 建模指南

如何把领域映射到 OKM 键空间：每实体四层、访问方法强制、覆盖索引克制、主键复合有边界。本指南是应用 schema 的规范性文档；机制细节见各 ADR 与 [README](../README.md)。

> **Languages:** [English](MODELING.md) (primary) · [中文](MODELING.zh-CN.md)

## 四层建模法

每个实体按 **命名空间 → 主键 → 排序字段 → 访问方法** 四层建模。

1. **命名空间（ns）**——实体活在哪个键空间。一类实体 = 一个 ns。2 字节 ns 已经区分了所有条目，不要把实体类型编码进 key。
2. **主键**——身份，且仅是身份。代理键（自增 id、UUID）是默认选择：属性是 payload，不是身份。
3. **主排序字段**——key 的第三段（`[ns][pkey...][order]`）。放 key 尾使同一 pkey 下条目物理相邻且有序，前缀扫描即时间线/区间读；其余排序维度不进 key，经 `#[kv_index]` 暴露。物理决策与间接成本的关系见下文「设计约束对性能的影响」。
4. **访问方法**——声明的 `#[kv_index]` 条目。每一个都是对"这个实体被怎么查？"的常备回答。

说不出访问方法，模型就没建完——说不出的查询会变成全表扫描。

## 声明基础

四层建模法在代码里的落点就是三个派生宏。完整声明词汇：

### 端点 key：`KeyEncode`

```rust
use okm::KeyEncode;

/// org 内的用户。org_id 是"组织前缀"，user_id 才是身份终点。
#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(1)] // 编译期命名空间，折叠为 key 的大端字节前缀
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(KeyEncode, Clone, PartialEq, Debug)]
#[kv_ns(2)]
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

### 边：`EdgeEncode`

```rust
use okm::EdgeEncode;

/// user → sessions 边。
///
/// 正向：user 的身份是 (org_id, user_id) 两个字段 → kv_head(org_id, user_id)
/// 反向：session 的身份是完整 SessionKey（无 kv_head）
///
/// 同一条边的两个方向使用不同宽度的端点身份——
/// 这就是"主键随方向变化"的表达。
#[derive(EdgeEncode, Clone)]
#[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}
```

`#[kv_head(field, ...)]` 声明该端点在**这条边里**哪些字段算身份；不标注 = 全量 key 即身份。字段名必须是端点声明序的前缀（宏生成的编译期检查）。声明一次，正反两族条目自动生成（方向位见上文「多对多关系」）。

### 行与索引：`RowEncode`

```rust
use okm::RowEncode;

/// 行 struct 挂在 UserKey 上（#[kv_ref]）；payload 字段 TLV 编码。
/// 每个 #[kv_index] 声明一个对 PAYLOAD 字段的访问方法——
/// 身份归 key（代理 id），业务维度归行。
#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(UserKey)]
#[kv_index(by_reputation { fields(reputation) })]
#[kv_index(by_org { fields(org_id, created_at), includes(bio_len) })]
pub struct User {
    pub org_id: u32,
    pub created_at: u64,
    pub reputation: u32,
    pub bio_len: u16,
}
```

- `#[kv_ref(UserKey)]`——行挂到哪个主键上；身份归 key，业务维度归行。
- `fields(...)`——排序/分组的 payload 字段，按声明序，首位 = 分组维度。
- `includes(...)`——覆盖索引，复制 payload 字段进 entry value（上文「覆盖索引克制」）。
- `key(...)`——把 entry 尾部携带的主键截断到命名子集（默认取满）。截断改变的是行级唯一性，不是分组：`fields` 前缀驱动排序，key 尾段区分行；`key(user_id)` 仅在命名子集对每行唯一时才安全，否则行会互相覆盖 entry。
- `func(path)`——函数索引：排序段由 `path(&row)` 的返回值编码（查询端探针调用同一路径归一化，一条声明驱动两侧）。返回单值 = 经典函数索引（如 `lower_name` 归一化）；**返回 `Vec<V>` = 多值函数索引**，一行展开为 N 条 entry——tokenize 全文检索、多值字段（tags）、时间分桶都落在这一原语上。多值时 token 即数据段（变长、贴主键前），读路径与普通索引完全相同（`scan::<I>`）。`okm` 不内置分词器，切分逻辑归业务层。
  实现细节（编码契约、entry_pairs 覆盖、探针归一化）见 internals 的[函数索引机制](internals/func-index-mechanism.zh-CN.md)。

索引条目物理布局（ADR-0005）：

```
[ ns 2B BE ][ slot 1B ][ 数据段（fields）BE ][ 主键前缀（默认取满） ]   value = includes 字段 TLV（无 includes 则为空）
```

判别符 = ns + slot：ns 段划整张表，slot 字节在表段内区分访问方法（主表 slot=0，索引按声明序 1, 2, …）；声明即注册，无运行时索引簿记。slot 按声明序机械分配（SLOT = `#[kv_index]` 出现的序位），因此**索引声明是 append-only 的**：只能在尾部追加，不能在中途插入或重排——插入会让其后所有索引的 slot 漂移，已落库条目留在旧 slot 位，`scan` 换了前缀后读到空结果（静默错误，不是变慢）。删除声明只是留下无害的 slot 洞（与 ns 编号永不复用是同一纪律，ADR-0002）。另注意：没有索引回填机制，尾部追加的新索引只对之后写入的行生效，存量行不补条目；需要覆盖存量时走迁移双写。

数据段（fields 段）的语义结构是**有序的维度序列**：首位是分组/等值维度，其后是排序维度，它受两重约束——
- **解码约束**：数据段中至多一个变长字段，且必须紧贴主键前缀之前。主键定宽（`KEY_LEN`），从尾部反推切出；其后若还有定宽字段也依次从右往左切；剩下整块就是那个唯一的变长段——它的长度不需要存储，边界由右侧定宽段反推。两个变长段（如 `fields(token, name)`）之间没有边界字节，解码不可能，derive 在编译期拒绝。
- **查询约束（leftmost-prefix）**：前缀扫描按数据段声明序从左往右完整指定。变长段不在末位时前缀语义即破——`fields(city_name, status)` 里裸字节 `"beijing"` 无终结符，会同时命中 `"beijing"` 与 `"beijing2"`。变长段必须排最后，等值/定宽维度排前面。

另注意 `includes` 不在数据段内——它在 entry value 里，不参与 key 结构与排序。想把"条目里多带点数据"表达成加 fields 段是建模误区，正确出口是 `includes`（免回表复制）或嵌套条目（存一起）。

### 连接、断开、查询（`EdgeTable`）

```rust
use okm::EdgeTable;

let store = okm::MockStore::default(); // 或 FjallStore / SlatedbStore
let mut edges: EdgeTable<_, UserToSessionEdge> = EdgeTable::new(store);

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
[ 头部 2B: (ns<<1 | dir) BE ][ A·身份 ][ B·身份 ]
```

`ns = 4`、FWD → 头部 `[0x08, 0x00]`；REV → `[0x08, 0x01]`。方向位 niche 在命名空间字段的最高位——见 [ADR-0001](docs/adr/0001-direction-bit-niche.md)。

### 反向查询与截断身份

```rust
// 反向：session → users。此方向 A 的身份是截断的（kv_head），
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

### 行的运行时用法（`Table`）

`RowEncode` 声明的访问方法在查询侧具名为索引类型。索引声明的派生物在展开点（本文件）生成：`kv_index(by_org ...)` 生成索引类型 `__OkmIndex_User_by_org`（机械拼接，无大小写转换），`use` 别名后即可作泛型参数。`Row::table` 构建装配点，调用处无需重复 key 类型：

```rust
use okm::{MockStore, Row};
use __OkmIndex_User_by_org as ByOrg; // 索引类型：kv_index(by_org) 的派生物

let mut t = <User as Row>::table(MockStore::default(), 9);

t.put(&user, &User { org_id: 7, created_at: 30, reputation: 100, bio_len: 2 });

// 任意访问方法上的最左前缀扫描，带回表：
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
for (key, row) in rows {
    // key: 解码的 UserKey，row: payload 存在时为 Some(解码的 User)
}

t.delete(&user); // 删除主键 + 所有已声明的索引 entry（多值索引逐条清除，无悬挂条目）
```

多值函数索引的声明与查询（倒排索引形态——token 是数据段，主键前缀从尾部反推）。func 返回 `Vec` 是契约而非偷懒：put 收到返回值后立即逐条写入，惰性迭代器没有收益，一次 `collect` 换 trait 面最小：

```rust
fn tokens(row: &Doc) -> Vec<String> {
    row.text.split_ascii_whitespace().map(String::from).collect()
}

#[derive(RowEncode, Clone, PartialEq, Debug)]
#[kv_ref(DocKey)]
#[kv_index(by_token { func(tokens) })]
pub struct Doc {
    pub text: String,
}

use __OkmIndex_Doc_by_token as ByToken;

// 写入：一行 "rust kv" 展开为两条 entry（rust → key，kv → key）
// 查询：token 前缀扫 → 回表，与普通索引无异
let hits = t.scan::<ByToken>(b"rust");
```

### Schema 稳定性测试

用硬编码 hex 锁定物理字节——任何布局漂移都让 CI 失败：

```rust
let fk = edge.forward_key();
assert_eq!(&fk[..2], &[0, 8]); // ns=4、FWD——方向位在 BE 字节对的最低位
assert_eq!(&fk[2..6], &7u32.to_be_bytes());
// ... 完整布局断言见 okm/tests/integration.rs
```

## 一对多关系

访问方法不只是排序：**一对多关系也能落成前缀分组**。`[tenant_id][org_id][user_id]` 这样的布局，前缀取一个组织内的全部 user——这正是关系模型里"外键 + 列表"的物理化。

OKM 里它以二级索引的形态出现：索引条目的布局是 `[ns 2B][slot][fields 段][主键前缀]`——`fields(org_id)` 是条目中部的分组/排序段（payload 字段按声明序编码，derive 生成、编译期布局锁定），提供分组前缀；`includes(...)` 是条目 value，复制常用字段免回表。注意这和主键布局无关：分组维度落在索引条目里，可组合、可下线，不存在"后缀无处解码"的问题。也可以直接把用户数据存进列表条目（嵌套式，SQL 里反范式化的"存一起"），存一起效率高，但用户与 org 从此锁定——跳槽要搬数据，与主键复合是同一陷阱。

对应 SQL 的范式化：独立 ns 是范式化的——parent_id、org_name 之类的组织属性只存组织行一份，用户行不带；存一起（反范式化）效率高但耦合，分开（范式化）多一次间接但身份单一。

## 多对多关系：边（edge）

`#[kv_index]` 定义的访问方法是**表内**的——数据源是本行 payload，随 `put`/`delete` 自动同步。跨表关系（一对多、多对多）是两个独立实体之间的事实，payload 索引够不着，由**边**表达：

```text
#[derive(EdgeEncode)]
#[kv_ns(4)]
struct OrgUserEdge {
    #[kv_head(tenant_id, org_id)]   // A 端身份截断到 (tenant_id, org_id)
    pub org: OrgKey,                // B 端无注解 = 完整 UserKey
    pub user: UserKey,
}
```

边物化为显式的正反双向 key（ADR-0001 方向位）：ns 顶位是方向位，FWD 前缀 `[ns:4][tenant][org]` 一次扫描取整组织的成员（列表侧），REV 前缀取某用户所属的全部组织；跳槽 = 增删一条边，身份不动。`#[kv_head(...)]` 声明该方向把端点身份截断到哪几个字段（不写 = 完整身份），让端点用更短的前缀、边 key 长度和分组粒度按方向各自裁剪——端点身份宽度的选取是"主键随方向变化"的表达。`kv_head` 选的是身份字段的子集（结构体声明过的字段），不是自由字节，可解性与索引尾段同理。

边与索引机制上同构（都是"指向身份的次级 key 布局"），但角色不能互换：

- **数据源**：索引的数据源是本行 payload，用户永不手写条目；边的数据源是两个实体的关系，是业务事实本身，必须显式双写双删。
- **端点**：索引尾段指向自己表的主键；边指向另一张表的实体（双端点、方向位区分正反）。

**双向是义务不是选项。** REV 条目只多付一份 key 的存储（LSM 顺序 append，最廉价的写），省掉它换来的却是：反查需求出现时全扫 FWD 段过滤（违反访问方法强制），或事后补边加回填迁移（贵几个量级）。与索引对照更清楚：索引只有一个方向，因为反查走主键 `get` 就行；边的两个端点都是次级视角，谁也不持有主键，所以两个方向都要一条。双向还让解绑变 O(1)——FWD/REV 两条 key 的身份都在手上，精确 `delete`，无需先扫后删。真正的克制点不在"要不要 REV"（不二选），在**要不要这条 Edge**：没有反查需求且基数小的关系（如配置类一对一），直接放 payload 字段就够，连边都不建；需要时再加，边的双写自动同步，无回填。

一句话：**索引 = 一行的派生视图，边 = 一等的关系数据**。`#[kv_index]` 定义时方便（声明即注册，无需手工编号 ns——ADR-0005 的动机正是消灭 per-index 手工编号与洞簿记）是次要红利，不是两者的分界；分界在数据源。

## 访问方法强制

查询只走 `get`（主键点查）或 `scan::<I>`（具名访问方法）。没有通用全表扫描的查询面。

仅有的裸扫描是 `scan_rows_raw` / `scan_keys`——它们是 snapshot 导出器与 Arrow bridge 的维护面，不是应用查询路径。应用代码伸手用它们，说明缺一条访问方法声明，不是捷径。

## 键的命名：朴素前缀 + 访问方法组合

键的命名就是三段：`[ns][pkey...][order]`。排序字段放 key 尾（u64 大端时间戳 = 时间序的字节序，天然时间线）：同一 pkey 下的条目物理相邻且按序排列，前缀扫描直接就是时间线/区间读——恢复会话、提取最近 N 条，都是一次 `scan`。主排序字段进 key 是物理决策；其余排序维度不进 key，放行 payload、经 `#[kv_index]` 暴露。

不建议花哨的前缀方案（路径式、语义分段式前缀）——前缀越长越难管理，每个前缀都是一条要维护的编码约定。宁可多声明访问方法：每个 `#[kv_index]` 是一条独立的、可组合、可扩展的读取路径，声明即注册，删一行即下线。

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

这些维度一律划到访问方法/二级索引：`#[kv_index]` 声明即注册，可组合、可扩展、随业务增长追加，无需改动主键布局。

## 两种尾段：索引尾可解，主键尾不可解

"key 尾部携带数据"有两种形态，可解性完全不同，容易混淆：

```text
索引条目  [ns 2B][slot][fields 段][主键前缀]   ← 尾段永远可解
主表条目  [ns][pkey...][自定义尾段?]       ← 自定义尾段无处解码
```

**索引尾段可解**：索引条目 key 的尾段就是主键编码——`#[kv_ref]` 声明过的结构体，`KeyEncode` 派生知道它的精确布局，从条目 key 末尾切出来即解回主键（`PrefixKey`，ADR-0005 的契约："the tail is always decodable from the last bytes of the entry key"）。`fields(...)` 段同理：payload 字段按声明序编码，编解码全是 derive 生成的编译期锁定代码。

**主键自定义尾段不可解**：主键 key 的布局由用户声明的 key 结构体决定，`decode` 只解声明的全长。key 尾塞一个结构体里不存在的"剩余标识"，任何生成代码都不知道它的偏移和宽度——`PrefixKey` 的契约还明确截断前缀之外是垃圾字节。

根源一句话：**尾段可解的前提是布局进了声明系统**。索引尾段（主键编码、`fields` 段）在声明系统内，所以可解；主键尾的"剩余标识"在声明系统外，所以无处解码——也不要塞，需求归 `fields(...)`。

术语约定：**"访问方法"（access method）是建模层的词**，指一条已声明的读取路径——建模指南用它；**"二级索引"（secondary index）是机制层的词**，指 `#[kv_index]` 生成的索引条目布局——ADR 与 README 用它。`#[kv_index]` 是两者的挂载点。本文档面向建模，正文一律用"访问方法"，提及机制布局时才落到"二级索引"。

## 设计约束对性能的影响

上述约束不是免费的，但代价有界，且换来的结构收益大于成本：

**key 里直接取 vs 访问方法取。** 剩余标识烧进 key，扫描时随条目免费带回（key 尾字节直接在手）；走访问方法，这个维度要经一次间接操作取回——`scan` 回表每条多一次主表点读加一次 `decode_payload` 解析，`scan_covered` 免点读但仍多一次 entry value 解析。单条确实多付一次解析。

**单次成本不高，摊销了。** 那一次间接是常规 LSM 点查/一次 TLV 解析，不在热路径的量级上；换来的是键布局不用为每个"还要按 X 查"的维度变形，业务增长时加 `#[kv_index]` 而不是重新设计 key。

**批量场景反而更优。** 回表看似逐条随机点查，实际两条流本身有序：索引条目按 `[pkey][order]` 聚簇有序，主表按 `[pkey]` 聚簇有序——回表天然退化为双指针归并（类似 merge join），顺序 I/O，远低于逐条随机读的开销。key 布局手工优化在批量场景并没有额外牌可打。

结论：访问方法的间接成本是一次可摊销的解析加一次可归并的有序读，用可控、有界的运行时开销，换掉键布局的不可控复杂度——这笔交换在业务复杂度上去之后只赚不赔。

## 覆盖索引克制

`includes(...)` 把 payload 字段复制进索引条目，`scan_covered` 因此免回表。这对高扇出读路径是真实收益——但每次 put/delete 都要付永久的写成本。

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

org 维度的读取走索引：`#[kv_index(by_org { fields(org_id, ...) })]` 用一次前缀扫描回答按 org 的读取，身份保持单一。

"用户产生的数据属于公司还是属于个人"这类归属问题同理——它们是业务问题，答案变了（数据迁移、所有权变更）身份不该变。这就是行模型（ADR-0006）的 payload-not-identity 规则，落成建模纪律。

## 演进

新的建模原则以小节追加到本文档；机制决策进 ADR 并引用，不重复。
