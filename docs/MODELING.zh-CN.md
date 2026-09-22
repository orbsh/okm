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

时间分桶是这个形状的最低成本用法——返回单元素 Vec，写入时把时间戳折叠成桶号（桶号定宽 BE，字节序 = 时间序），按小时的 rollup/时间线就是一次前缀扫。同一原语直接覆盖 tokenize 全文检索（切词返回 Vec<String>）、多值字段（tags 拆分）。\`okm-core\` 不内置分词器，切分/分桶逻辑归业务层；func 的契约是**纯函数**——delete 从行重新生成待删集合，函数不纯（时钟/随机/外部状态）会在删除时生成与写入时不同的集合，留下悬挂条目。空 `Vec` 也是一种行级过滤（该行不产生任何 entry），但行级条件写成声明上的 `where` 更清楚。

**基础用法三：部分索引（`where` 谓词）。** 只索引被谓词接纳的行——存量里绝大多数行用不到该访问方法时（例如工单 99% 已关闭，只索引未关闭的），条目量与维护成本按谓词后的子集计：

```text
fn is_open(row: &Ticket) -> bool { row.status == 0 }

#[ok_index(open_by_assignee {
    fields(assignee_id, created_at),
    includes(title_len),
    where(is_open),
})]
```

谓词与 `func` 同级，同受纯函数契约约束：delete 从行重新派生待删集合，谓词读时钟或外部状态会让删除生成与写入不同的集合，留下悬挂条目。谓词的保证到写入侧为止——条目按索引字段定址，谓词只决定"这次写不写"，所以 put 覆盖写不清理旧条目：行从被接纳入索引变为被拒绝时，旧条目悬挂（与「值变化留下悬挂条目」同一既有契约），清理要用**当时生成它的那一行**去 delete。读侧没有谁替你校验完备性——部分索引只对"查询条件蕴含谓词"的查询是全的，okm 没有查询优化器，用哪个索引由调用方决定，这条纪律属于建模，不属于机制。

`func(path)` 与下文的 `#[ok_reduce]` 是两个外部扩展机制：func 是**单行派生**（写侧预计算，entry 仍随行生灭），reduce 是**跨行聚合**（可变 value 读-改-写）；集成的复杂形态（FTS/向量/图算法如何落在原语上）见[集成边界](integration/EXTENSION-TYPES.zh-CN.md)。实现细节（编码契约、entry_pairs 覆盖、探针归一化）见 internals 的[函数索引机制](internals/func-index-mechanism.zh-CN.md)。

索引条目物理布局（ADR-0005）：

```
[ ns 2B BE ][ slot 2B BE ][ 数据段（fields）BE ][ 主键前缀（默认取满） ]   value = includes 字段 TLV（无 includes 则为空）
```

判别符 = ns + slot：ns 段划整张表，slot 的段号区分条目类别（主表/索引/reduce/junction，ADR-0016）；声明即注册，无运行时索引簿记。slot 按声明序机械分配（SLOT = `#[ok_index]` 出现的序位），因此**索引声明是 append-only 的**：只能在尾部追加，不能在中途插入或重排——插入会让其后所有索引的 slot 漂移，已落库条目留在旧 slot 位，`scan` 换了前缀后读到空结果（静默错误，不是变慢）。删除声明只是留下无害的 slot 洞（与 ns 编号永不复用是同一纪律，ADR-0002）。另注意：没有索引回填机制，尾部追加的新索引只对之后写入的行生效，存量行不补条目；需要覆盖存量时走迁移双写。

数据段（fields 段）的语义结构是**有序的维度序列**：首位是分组/等值维度，其后是排序维度，它受两重约束——
- **解码约束**：数据段中至多一个变长字段，且必须紧贴主键前缀之前。主键定宽（`KEY_LEN`），从尾部反推切出；其后若还有定宽字段也依次从右往左切；剩下整块就是那个唯一的变长段——它的长度不需要存储，边界由右侧定宽段反推。两个变长段（如 `fields(token, name)`）之间没有边界字节，解码不可能，derive 在编译期拒绝。
- **查询约束（leftmost-prefix）**：前缀扫描按数据段声明序从左往右完整指定。变长段不在末位时前缀语义即破——`fields(city_name, status)` 里裸字节 `"beijing"` 无终结符，会同时命中 `"beijing"` 与 `"beijing2"`。变长段必须排最后，等值/定宽维度排前面。

另注意 `includes` 不在数据段内——它在 entry value 里，不参与 key 结构与排序。想把"条目里多带点数据"表达成加 fields 段是建模误区，正确出口是 `includes`（免回表复制）或嵌套条目（存一起）。

### 连接、断开、查询（`Junction`）

```rust
use okm_core::Junction;

let store = okm_core::TestStore::default(); // slatedb-mem；另有 FjallStore / SlatedbStore / RedbStore
let mut edges: Junction<_, UserToSession> = Junction::new(store);

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

let mut t = <User as Document>::collection(TestStore::default());

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

一套编码同时服务声明行与外部数据。声明字段照常走热/冷段；其余落进**动态段**（slot 1），以 n-TLV 帧——`[字段编号][类型][长度][字节]`——存储，名字在**字段名字典**（slot 2/3）里首次出现时分配。`Collection` 上的运行时接口：

```rust
// 行-映射桥：全部字段（声明 + 动态）lift 到逻辑类型
// （Quant -> F64、VarInt -> u64、Enum -> 变体名、Offset -> i64）。
let fields = t.get_document(&key);          // Option<BTreeMap<String, DynamicValue>>，None = 行不存在

// 整体写：名字匹配声明结构体的字段走 typed 路径；
// 未知名字在字典里分配编号，落进 slot 1。
t.put_document(&key, &fields);                      // BTreeMap<String, DynamicValue>

// 仅动态段的视图（直接操作 slot 1）：
t.get_fields(&key);                             // 名字键 map 或 None
t.put_fields(&key, &map);                       // 整段替换，缺席字段被移除
t.delete_fields(&key);                          // 清空动态段
t.delete(&key);                                   // 移除 slot 0 + 全部索引条目
```

- `DynamicValue` 承载开放的值词汇表：`UInt`/`Int`/`F64`/`Str`/`Bytes`/
  `Bool`/`Null`/`Array`/`Obj`——嵌套对象以原生帧递归（类型标签 7），
  共享所在表的字典；不依赖 CBOR。
- `Bytes` 是词汇表中的**不透明成员**：存储层不解释任何东西——只有
  tag 与总长度；内容及其含义属于应用。不透明说的是**内容**：帧的
  `[len]` 前缀是 payload 定界（编码层的职责——多字段载荷靠它切分），
  不是对内容的解释；`String` 带同样的头。它是扩展类型的标准逃生舱：
  新编码先以 Bytes 形态落地，未来提升为一等类型是纯增量变更。
  字段名承担 tag 刻意不承担的语义（`embed_v1`、`attrs_cbor`）。
- 未知名字在这里是**正常输入**（外部数据、MQ payload）；「未知字段拒绝」
  纪律只适用于声明路径的 typed 解码器。
- 声明字段可索引；动态字段不可（名字是运行期数据）。
- schema 导出（`CollectionSchema`，serde 在 `schema-serde` feature 后）驱动
  嵌入式语言读取器的动态 codec——Python（PyO3）与 Steel binding 在
  `bindings/`，与 Rust derive 字节一致（交叉测试锁定）。版本默认值迁移
  在动态读路径同样生效：字面量 `#[ok_default]` 随 schema 走。

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

Junction 覆盖的多对多绑定在**编译期端点类型**上。知识图谱打破这个绑定：一个图连接多种类型的节点，边 kind 是运行期数据，平行边是不同的事实，边还携带属性。图边是第三种关系载体（ADR-0017）——独立身份、属性、平行边、开放端点。

节点就是普通 document collection：节点的"类型"是它的 collection（ns 即类型标记），节点 kind / 属性过滤走 collection 自己的声明索引和字典——与任何文档 collection 布局零差异。新增的东西都在边这一侧：

```rust
use okm_core::{EdgeEncode, EdgeFact, Graph, NodeRef};

/// 一个图的边 collection。声明属性字段是边自身的数据；端点不需要声明——
/// 引用自描述（[ns 2B][len][pkey]），任意节点 collection 零成本参与。
#[derive(EdgeEncode, Clone, Default)]
#[ok_edge(ns = 100)]
struct Employment {
    since_year: u16,   // 每个声明字段 = 一条 0x1 面
    weight: u32,
}

let mut g: Graph<_, Employment> = Graph::new(store);
let attrs = Employment { since_year: 2020, weight: 7 };
g.link(&EdgeFact {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    kind: "employs".into(),
    attrs: okm_core::KvGraph::attrs(&attrs),
}, &attrs, 1).unwrap();

g.typed_out("employs", &user1);   // 0x7 面：类型限定遍历
let slot = __OkmEdgeIndex_Employment_since_year::SLOT; // derive-generated 0x1 face slot
g.by_attr_face(slot, &2020u16.to_be_bytes());
```

- **端点引用**是 `[ns 2B][len varint][pkey]`——ns 充当类型标记，pkey 宽度以单字节 varint 住在引用自身。任意 collection 的文档免声明参与：同节点 = 同字节（遍历面精确前缀命中），不同 pkey 宽度共存，没有需要声明、维护、防错的注册表。图边让 Ref 纪律退役——引用自带宽度，连同 ns 知识一起。
- **面**：一次 `link` 写主表（slot 0x0，`edge_id` u64）+ kind 索引（0x4）+ 出入遍历（0x5/0x6）+ 类型限定遍历（0x7/0x8）+ 每个声明字段一条属性面（0x1），一个 batch；`unlink` 对称删除。kind 名走边 collection 自己的字典（0x2/0x3，先见者得号）。
- **平行边**是独立事实：每次 link 携带调用方选定的 `edge_id`；live id 被拒绝，绝不复用。
- **过滤**：先走选择度最高的面，id 在内存收敛——默认不建复合面（`(*)-[kind]->(:node_kind)` = 0x7 扫描与节点 kind 面的 pkey 集合求交）。

分界线：`Refs` = 一对多；`Junction` = 多对多、固定端点、无属性；**图边** = 独立身份、属性、平行边、开放端点。

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

元素是**纯值**——身份分界线（上文多对多一节）把 Vector 挡在关系载体之外：标量没有 key，对它做 key 引用是范畴错误。一对多 → `Refs`；多对多 → `Junction`；表达顺序的值列表 → `Vector`；异质动态列表 → `DynamicValue::Array`（slot 1）。`get_document` 时 Vector 提升为 `DynVal::Array`——动态层没有 Vector 类型，Vector 是存储层格式。多维形状是应用层解释（扁平序列按行优先），wire 上什么都不存。

消费侧（okm-vector）在其上构建搜索：帧字节就是 function-index 的数据段（前缀扫描做精确匹配/量化桶召回），距离是 rerank 侧的事。存储保持字节不透明。

## 跨行预聚合

索引的每个条目都是**随行生灭**的 append-only 派生视图——delete 用同一个函数重新生成待删集合，永不悬挂。有一类需求天然落在这个纪律之外：按作者统计发文数、按小时的 rollup、精确计数器。它们是**跨行**的——func 的签名 `fn(&Row)` 只看得见一行，答案落在 entry value 的**读-改-写**上，即第四个原语（可变聚合 entry）。

okm-core 对它的立场是两层拆分：核心不内置任何聚合语义（没有内建计数器类型，不解决分布式累加协议），但机械部分由辅助设施提供，声明方式与索引同款：

```text
#[ok_reduce(AuthorStats { group(author_id) })]
```

`group(...)` 从行字段取分组段（entry = `[ns][slot][group 段]`，slot 续接索引计数器）；`AuthorStats` 是用户类型，实现 `ReduceLogic`——`Acc`（累计器类型，实现 `ReduceCodec` 定宽 BE 编码）+ `fold(acc, &row)`（put 时）+ `unfold(acc, &row)`（delete 时）。写入路径自动读-改-写：读到当前 acc，fold/unfold，写回。读侧 `reduce_get` 取单组、`scan_reduces` 扫全部组。机制细节（账本不变量、覆盖写的 unfold 补偿、写路径时序）见 internals 的[reduce 机制](internals/reduce-mechanism.zh-CN.md)。

两条使用纪律：

- **可逆性是契约**：`unfold(fold(a,x)) = a` 必须精确成立，count/sum 可以，median/distinct 不可以——不可逆聚合去旁边的 OLAP 系统。复合 acc（count+sum 求均值）直接实现 `ReduceCodec`，okm-core 只负责存取字节。
- **单写者边界**：hook 是读-改-写，单写者引擎下安全；多写者竞态与分布式累加协议不在此模型内（见集成文档）。

零值回收刻意不做：组空了 entry 仍在（acc 回到单位元），省掉墓碑逻辑；调用方按需跳过单位元组。

## 两种读-改-写：reduce 与 upsert_with

reduce 之外，写路径还有命令式的一半——`Collection::upsert_with(key, f)`：读旧行、闭包算新值、走正常 put。两种 RMW 同一底层形态（read → compute → write），分工按「逻辑谁知道」切：

- **reduce（声明式）**：`#[ok_reduce(Logic { group(f) })]` 在编译期定死——哪些行进哪个组、fold/unfold 怎么算，都是类型声明的一部分，框架驱动。适合与行结构同步演化的聚合（计数、求和）。
- **upsert_with（命令式）**：`f(Option<R>) -> R` 在运行时收到旧行，任意逻辑。适合调用方才知道的更新（余额加减、条件修补）。`None` = key 不存在（插入路径）。

两者共用同一条写路径（put），所以索引维护、reduce 折叠、事件发射全部照常触发，无特例。也共用同一个正确性边界：**单写者**。OKM 是进程内库、写序串行（`&mut self`），get→f→put 不可能交错——无需 CAS，这也是 reduce 恰好一次的同一约束。多写者未来（乐观 CAS）是不同机制，不在此模型内。覆盖写的 unfold 补偿由 put 内部完成，upsert_with 不另平账（见 internals 的 reduce 机制）。

## 写路径事件：inline 与 channel

reduce 回答"累计后的状态长什么样"；另一类消费者需要的是写本身作为事件——缓存失效、搜索索引同步、下游通知。事件层（ADR-0008）把这类消费者拆成两种，这个拆分就是全部设计：

- **Inline**（reduce）：运行在写路径内部，构造上恰好一次——fold 就是写的一部分。reduce 永远不消费 channel。
- **Channel**（`#[ok_subscribe]`）：best-effort 投递，无保证。注解声明"该行类型的写路径事件进入 channel"；注解处没有 handler——处理逻辑完全归消费者：

```text
#[ok_subscribe]                      // bare：唯一形态；事件 enum 由 build.rs 推导
                                     // （variant = 行类型名，enum 名可用
                                     // #[ok_event_enum(Alias)] 覆盖）
```

事件携带 `op`（put/delete）、单调递增的**写批次 epoch** 和行本身。epoch 是发出这张表的写计数器：同一张表的事件带精确的同表批次边界，消费者组合子可以恰好折叠到边界为止（glitch-free），而不是靠去抖启发式。它只在进程内有意义——不持久化，重启归零——并且不提供跨表顺序：独立 put 之间不存在原子性的"两者都已更新"时刻，多表 fan-in 结构上就是最终一致。

两条使用纪律：

- **正确性永不放上 channel。** 队列满会丢、没有 sink 会静默丢；必须恰好一次发生的事（如 reduce）属于 inline。channel 的位置是容忍丢失、事后可对账的消费者。
- **纯度约束同样适用于事件载荷**：行快照按原样随事件走；消费时再去派生额外上下文，等于重读一个事件已不再保证描述的存储。

传输是组装点决策（`ChannelCell::register` 接受任意 sink——tokio mpsc、crossbeam 队列、no-op）；core 保持同步、不认识任何 executor。对 receiver 的组合子（map/filter/merge/折叠到 epoch 边界）属于 stream 层的职责，不属于模型层。

## 访问方法强制

查询只走 `get`（主键点查）或 `scan::<I>`（具名访问方法）。没有通用全表扫描的查询面。

仅有的裸扫描是 `scan_rows_raw` / `scan_keys`——它们是 snapshot 导出器与 Arrow bridge 的维护面，不是应用查询路径。应用代码伸手用它们，说明缺一条访问方法声明，不是捷径。

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

**key 里直接取 vs 访问方法取。** 剩余标识烧进 key，扫描时随条目免费带回（key 尾字节直接在手）；走访问方法，这个维度要经一次间接操作取回——`scan` 回表每条多一次主表点读加一次 `decode_payload` 解析，`scan_covered` 免点读但仍多一次 entry value 解析。单条确实多付一次解析。

**单次成本不高，摊销了。** 那一次间接是常规 LSM 点查/一次 TLV 解析，不在热路径的量级上；换来的是键布局不用为每个"还要按 X 查"的维度变形，业务增长时加 `#[ok_index]` 而不是重新设计 key。

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

org 维度的读取走索引：`#[ok_index(by_org { fields(org_id, ...) })]` 用一次前缀扫描回答按 org 的读取，身份保持单一。

"用户产生的数据属于公司还是属于个人"这类归属问题同理——它们是业务问题，答案变了（数据迁移、所有权变更）身份不该变。这就是行模型（ADR-0006）的 payload-not-identity 规则，落成建模纪律。

## 演进

新的建模原则以小节追加到本文档；机制决策进 ADR 并引用，不重复。
