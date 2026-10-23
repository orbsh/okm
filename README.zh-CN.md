# OKM — Object-Keyspace Mapping（对象键空间映射）

> ORM 的体验，Redis 的速度，PostgreSQL 的持久，不受边界限制的功能。

> 英文版为主文档（[README.md](README.md)），本文为对应中文版。

OKM 是对标 ORM 的范式——逻辑定义是 ORM 式的，代码生成产出底层 KV 操作。ORM 将对象映射到关系表，OKM 将对象映射到 KV 键空间。通过派生宏（`KeyEncode` / `DocumentEncode` / `JunctionEncode` / `GraphEncode`）+ 数字命名空间 ID，构建零成本抽象语义数据层：开发侧如同 ORM 般声明式，编译后退化为纯指针偏移计算。

关联阅读：[KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) — 底层架构与设计模式（编码原理、索引策略、引擎层取舍）；[架构总览](docs/ARCHITECTURE.zh-CN.md)——双层声明机器（derive 展开 + 常量表）；[建模指南](docs/MODELING.zh-CN.md) — 规范性 schema 建模方法（四层建模法、访问方法强制、覆盖索引克制、主键复合边界）；[查询配方](docs/query-recipes.zh-CN.md)——把 merge_join / group_by / walk 组合成 SQL 形状，WHERE 边界记录；[图查询配方](docs/graph-queries.zh-CN.md)——图的面组合成 Cypher 形状的遍历；[并发与写者](docs/CONCURRENCY.zh-CN.md)——单写者的确切含义、哪些机制依赖它、以及四种能承载更多写者的形态。

## 为什么：代码即 DDL

SQL 的核心价值不是执行性能，而是关系模型交付的可读性、建模规整度和团队协作确定性。裸露的二进制 Key 会退化为面条代码——团队无法理解 Key 的排列规则。解决方案：用 Rust 类型系统替代 SQL DDL，把 Schema 正确性从运行时数据库引擎提到编译期编译器。

- **Struct 即 DDL**：SQL 定义表结构，Rust Struct 定义 Key 编码。编译器保证格式一致——任何试图写入错误格式 Key 的代码在编译期直接报错，不需要运行时校验。
- **无模式是陷阱**：无模式存储来者不拒（数字字段可以写成字符串 `"25"` 甚至数组 `[25]`），代价是所有消费端代码写满防御性解析。强类型把脏数据在 `cargo check` 阶段熔断——连生成的资格都没有——同时保留 KV 引擎的硬件级速度，免掉 PG 的运行时 DDL 锁开销和 SQL 字符串解析税。
- **Hex 稳定性测试**：编码漂移的最终防线。硬编码历史 hex 字节锁定物理 Key 布局；任何改动 ns、字段顺序或宽度的行为都让 CI 立刻失败（见 `okm-core/tests/integration.rs`）。

判定：SQL 的核心价值是面向人类的结构化纪律。KV 只要贯彻「代码即 DDL 的强类型编码 + 多版本 payload 懒迁移 + 双写 Key 指针契约 + hex 硬编码单元测试」，就同时获得了：编译期 Schema 安全（Rust 编译器）+ 运行时极致性能（LSM-Tree）+ 零停机演进（版本化 payload）+ 团队可维护性（Struct 注释即文档）+ 编码漂移防护（hex 单元测试）。在 Schema 安全性上完成对 SurrealDB（无模式）和 PostgreSQL（运行时 DDL 锁）的双向反超。

## 两种模式：编译期 schema 与声明式 schema

OKM 只有一个存储引擎契约、一套键布局，但有两个 schema 载体：

- **静态模式（代码生成）**——Rust 类型 + `#[derive(...)]`（okm-derive）。编译器即 schema 校验器：键宽、偏移、索引声明都是编译期事实；组装点是 `Collection<S, K, R>`。面向 Rust 宿主与 Rust 源码编译的 wasm 摊位。
- **动态模式（schema 数据）**——一个 `CollectionSchema` 值（静态侧经 `okm_core::schema::CollectionSchema::of` 导出，或直接以数据形式编写）+ okm-dynamic。`DynamicCollection` 接受运行时 ns 与 schema 值；编解码与 derive 逐字节一致。面向嵌入式语言 摊位（经 bindings 的 Python/Steel）与运行时组装 collection 的宿主（aura ADR-0026：每 摊位 类型一个真实 ns，从注册表分配）。

两种模式对同一声明产生**完全相同的字节**：同一 header 纪律、同一键编码、同一 payload 帧、同一词汇表行为。编译期 collection 写的数据，动态侧读得回来，反之亦然——模式是**写入方**的属性，不是数据的属性。写动词刻意对齐（`merge` / `replace` / `purge` / `scan` + document map），应用代码形状不分叉；typed 侧的 `get` / `put` 是说结构体的那一道门。

crate 分工：okm-core = 共享运行时（引擎契约、`Collection`、document/动态段机制、reduce、subscribe）；okm-derive = 静态代码生成；okm-dynamic = schema 驱动编解码与 `DynamicCollection`；okm-wire = 远程引擎 wire 格式；okm-query/stream/graph/vector/ngram = core 之上的消费侧算子。

## 物理收益

- **85% 前缀压缩**：14 字节字符串前缀 → 2 字节数字命名空间。
- **100% 定长 Key**（全量身份）：所有字段的绝对字节偏移被编译期死锁，反序列化纯指针切片，零解析。
- **32768 个命名空间**：u16 的最高位 niche 为边方向位（见 [ADR-0001](docs/adr/0001-direction-bit-niche.md)）。
- **缓存局部性**：LSM-Tree 布隆过滤器拦截 + 内存 Seek，热路径 CPU 缓存局部性极好。
- **零运行时开销**：无正则/split/AST——查询路径随 `cargo build --release` 固化为机器码。

## 状态

已实现：

- `KeyEncode` — 定宽 key 编码（`u32` / `u64` / `[u8; N]`），大端序，编译期 `KEY_LEN` / `FIELD_WIDTHS`，`encode_prefix_named` 截断原语。
- `JunctionEncode` — junction（SQL 多对多连接表）：每端 ns 一条单向条目，4 字节头 `[ns u16][slot u16]`（0x3 段），各端点身份宽度可独立声明（`#[ok_head(...)]`），`#[ok_junction(n)]` 区分号，查询方法生成在端点 key 类型上。
- `Junction<S, E>` — junction 装配点：引擎 + junction 类型 = 一条关系的操作面（`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`）。
- `GraphEncode` + `Graph<S, E>` + `NodeSet`/`NodeRef` — 图边，第三种关系载体（ADR-0017）：**一份声明就是这张图**（ns + 边自己的载荷 + 显式 `#[ok_index]` 面 + `#[ok_nodes]` 成员集），开放端点走自描述引用 `[ns 2B][len][pkey]`（pkey 宽度住在引用自身——无注册表），平行边携带调用方选定的 id，谓词走图自己的谓词词汇表（0x04/0x05——列举就是一扫前缀），声明属性面就是普通计数族 0x1 索引；`link`/`unlink` 一个 batch 写/删全部面，`unlink` 解码载荷后精确删除；无类型/谓词/谓词过滤/属性四种扫描。
- 引擎后端走 Cargo feature：`fjall`（同步 `FjallStore`）、`redb`（同步 `RedbStore`）、`slatedb`（异步 `SlatedbStore` + `AsyncJunction`）；测试用 `TestStore` 引擎矩阵（slatedb 内存 / fjall / redb）跑真实引擎，没有会漂移的 mock。

- 二级索引（访问方法）——**行 struct** 上的 `#[ok_index(name { fields(…), includes(…), key(…), func(…), where(…) })]`：对 **payload 字段**（按声明序）建组合索引；无 per-index slot/ns——2 字节表命名空间已区分所有 entry；最左前缀扫描；`key(…)` 把 key 尾部携带的主键截断到命名子集（`encode_prefix_named`），默认取满主键；`includes` 覆盖索引定位为高扇出查询的物化视图；`func(path)` 用普通 fn 的返回值替代数据段（探针端调同一路径）；`where(path)` 是行级谓词——部分索引，被拒绝的行不产生任何条目。
- `Collection<S, K, R>` 行装配点——`put`/`purge` 在同一 store 实例内一次写入（或删除）主键与全部声明的索引条目（声明即注册表）；`scan` 经任意访问方法的最左前缀返回 `(Key, Option<Row>)`。
- **语义化 batch 写路径**（[ADR-0031](docs/adr/0031-batch-write-semantics.md)）——`BatchStore`（把 batch 当成写入器：点读先看本批待写项、scan 一律拒绝）+ `Collection::save_with_prev_into(batch, key, doc, prev)`：主键 + 面写进调用方的 `Batch`，reduce 的折/回折经适配器完成，新文档不再产出的面一并删除——带账本的表从此能加入跨表原子提交，而不必自己重写一份账本。
- 字段级编码 wrapper（`Vocab<N>`、`Offset<T>`、`VarInt<T>`、`Quant<P>`、`Reverse<T>`、`Vector<T>`、`Bytes`）与变长载荷/索引字段（`String`），key 保持定宽。
- 多引擎混用——同一进程内不同 ns 段可绑不同引擎（交易走 fjall、日志走 slatedb）；原子性止于单引擎内，ns 编号全库唯一。
- 快照导出——行 → Parquet，与引擎无关（备份 / 数据交换 / lakehouse 分析）；ns 还原为描述性文本，列名即字段名。
- **Document API**——两道读门、三个写动词。结构体门：`get` / `put` 说 `R`（只有 typed 侧有）。文档门：`fetch` 读整份文档（两槽合并的名字键 map）、`merge` 只动点名的名字、`replace` 让 map 就是文档、`purge` 清掉两槽；每次写一个引擎提交。动态侧只有文档门——那边没有结构体可说。未知字段名在每表字段名词汇表里分配编号，落进动态段（n-TLV 帧、嵌套对象递归、无 CBOR）；行-映射桥把声明字段 lift 到逻辑类型（`Quant`→`F64`、`Vocab`→标签、`Offset`→`i64`）。
- **动态 codec binding**——schema 导出驱动嵌入式语言读取器：`bindings/okm-python`（PyO3）与 `bindings/okm-steel`，与 Rust derive 字节一致。
- **动态段上的路径索引**（[ADR-0029](docs/adr/0029-path-index.zh-CN.md)）——`AccessMethod::path(slot, Path::parse("author.name")?, FieldType::Str)?`：把嵌套路径与数组路径声明成访问方法，每个解析出的值一条条目（通配 = 没有 `Index` 组件），维护挂在动态段写路径上；值类型随声明，条目键携带 slot、从不携带路径。
- **Document 模型**——声明式 row 与外部数据共用单一编码（[ADR-0012](docs/adr/0012-object-model-and-field-dictionary.md)）。OKM 面向文档：document 逻辑上全动态（运行期可出现任意字段），且**声明的静态字段嵌入动态整体**——hot/cold 段、索引、schema 导出照常生效。声明 struct 是只有静态路径的退化 document；纯 document（MQ payload、JSON）是只有动态路径的退化 document；两者同一编码。嵌入光谱上的术语：
  - **variant**——动态内容嵌为一个声明的静态字段（`Bytes` payload）；动态性活在值里，不进键布局。

  动态帧采用 CBOR 衍生的取帧方式，不是 CBOR 本身：只取 **major type 模式**——每帧以类型 nibble 开头——结构编码也在同一方案里。CBOR 的 major type 含列表和映射；有了字段名词汇表，OKM 只需要列表：映射就是 nTLV 列表（编号、类型、长度、值），结构信息活在值里，不进 per-document schema。

## 使用方法

### 1. 声明：key、行与关系

完整的声明词汇（`KeyEncode` / `DocumentEncode` / `JunctionEncode` /
`GraphEncode`，以及 `#[ok_index]` 的 `fields`/`includes`/`key`/`func`/`where`
注解）见[建模指南](docs/MODELING.zh-CN.md)「声明基础」。四种形态：

```rust
#[derive(KeyEncode)]                        // 身份：定宽字段、大端、无分隔符
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode)]                   // 行：身份 + 载荷 + 访问方法
#[ok_ref(UserKey)]                          // 行自己钉住 key 类型
#[ok_ns(1)]                                 // ns 声明在这里，不在组装处
#[ok_index(by_org { fields(org_id) })]      // 一个面：条目键 = 被索引字段 + key 尾段
pub struct User { pub org_id: u32, pub bio_len: u32 }

#[derive(JunctionEncode)] #[ok_junction(1)] // 多对多，端点强类型
pub struct UserToSession {
    #[ok_head(org_id, user_id)]
    pub user: Ref<User, UserKey>,
    pub session: Ref<Session, SessionKey>,
}

#[derive(GraphEncode, Clone, Default)]       // 一份声明就是这张图（ADR-0017）
#[ok_ns(100)]
#[ok_nodes(User, Org)]                       // 成员集：节点**表**，按类型列出
#[ok_index(by_since { fields(since_year) })] // 属性是显式声明的面——不做任何隐含
pub struct Employment { pub since_year: u16, pub weight: u32 }

// 生成：`pub enum EmploymentNode { User, Org }` 及其 `NodeSet` 实现，
// 每个 ns 取自那张表自己的 `#[ok_ns]`——不写两遍
```

索引声明机械地生成访问方法类型：`ok_index(by_org ...)` 产出
`__OkmIndex_User_by_org`（无大小写转换），`scan::<…>` 要的就是它——
`use __OkmIndex_User_by_org as ByOrg` 给出短名。

### 2. 运行时：文档（`Collection`）

```rust
use okm_core::{Collection, Document};

let users = <User as Document>::collection(store.clone());
let key = UserKey { org_id: 7, user_id: 101 };

// 写：主表条目 + 每个声明的面一条，同一个 store 实例。
// 动词取 &self（与底层 KV 的 put/del 一致），独占性由集合 !Sync 保证。
users.put(&key, &User { org_id: 7, bio_len: 240 });
let row: Option<User> = users.get(&key);

// 按访问方法读：最左前缀扫描 + 回表。
for (pk, row) in users.scan::<ByOrg>(&7u32.to_be_bytes()) {
    // pk.decoded: UserKey，row: Option<User>
}

// 索引首字段上的比较就是一段物理键区间——字节序即值序，
// 所以这里只读 [1, 100) 内的行。
users.scan_range::<ByOrg>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()));

// 删：主表 + 恰是这份文档产出的那些面。
users.purge(&key);   // 主条目 + 面 + 动态段，一次提交；删除集合派生自存储行
```

覆盖语义按设计分成两半：`put` 先把已存文档从每个声明的 reduce 组里回折，
再折入新文档；而面是逐文档派生——被索引的值一改，条目就落到新键上，旧面
由调用方负责删除（或者交给 `save_with_prev_into`，见第 5 节）。

账本是一行声明，不是你自己维护的一张表（ADR-0023）：

```rust
#[ok_reduce(Count { group(org_id) })]          // 每组的行数，精确可逆
#[ok_reduce(Sum(bio_len) { group(org_id) })]   // 字段求和；有符号与 Quant 保持精确
```

累加器条目是 `[ns 2B][reduce slot][group 段]`，由写行的同一个 `put`/`purge`
读-改-写——同一个 store 实例，所以账本与行共享同一个原子性边界。

### 3. 动态字段：document API

两组动词，区别在“这次调用能看到什么”，不在方便与否：

| 动词 | 看到 | 付出 |
|---|---|---|
| `get` / `put` | 只有声明行（`R`），永远不碰动态段 | 不载入字段名词汇表，不读段——编译期 schema 这条路 |
| `fetch` / `merge` / `replace` | 整份文档：一张按名字索引的 map，覆盖两半 | 只读这次点到的那些臂 |

`get` / `put` 是纯静态模式下的优化，不是遗留角落：行就是结构体，而结构体就是它的全部——
`put(&key, &R)` 根本说不出声明之外的名字。它们从不载入字段名词汇表，也从不读或写动态段。

`fetch` / `merge` / `replace` 是通用形态，给带开放字段的表用。`merge` 在规划之前
先分类：声明名走 typed 那条（行 RMW、面、账本、事件），其余名字进动态段——
没有声明名的 patch 不读行，没有动态名的 patch 不读段、也不载入词汇表。`replace`
则把 map 就当成文档（没点名的声明字段取默认值，没点名的动态名被删掉）。
`purge` 是两侧唯一的删除动词。

一套编码服务声明行与外部数据。未知字段名是正常输入：在每表的字段名词汇表里分配编号，落进动态段（`[id][type][len][value]` 帧、嵌套对象递归）。声明字段保持 typed 且可索引。

```rust
// 读：声明字段 lift 到逻辑类型，叠在动态字段之上。
// 一张按名字索引的 map——这是文档本身，不是"行 + 附件"。
let obj = users.fetch(&key)?;                 // BTreeMap<String, DynamicValue>

// merge：命中声明的名字走 typed 路径（索引、reduce、事件一并生效），
// 未知名字并入动态段。只动点名的字段，其余保持库里已有的值。
users.merge(&key, &patch);

// replace：map 就是文档（缺席的声明字段取默认，未点名的动态名被移除）。
users.replace(&key, &obj);

// purge：两槽一起清，一次提交。
users.purge(&key);
```

嵌入式语言读取器（Python / Steel 摊位）经 schema 导出消费动态 codec——`bindings/okm-python`（PyO3）与 `bindings/okm-steel` 与 Rust derive 字节一致；版本默认值迁移在该路径同样生效。

### 4. 运行时：关系（junction 与图）

```rust
// junction：每端 ns 一条单向条目（共两条），双向都能查。
let edges: Junction<_, UserToSession> = Junction::new(store.clone());
edges.link(&user, &session);                  // 正向 + 反向，一个实例内
let sessions = user.get_session(&edges);      // 方法生成在 key 类型上
edges.unlink(&user, &session);

// 图：端点是自描述引用——没有注册表，没有宽度表。
let mut g: Graph<_, Employment> = Graph::new(store.clone());
g.link(&EdgeHead {
    src: NodeRef::new(&10u16.to_be_bytes(), &1u64.to_be_bytes()),  // [ns][len][pkey]
    dst: NodeRef::new(&11u16.to_be_bytes(), &9u64.to_be_bytes()),
    predicate: "employs".into(),                                  // 运行期数据，进词汇表
}, &Employment { since_year: 2020, weight: 7 }, 1)?;             // edge id 由调用方给

g.out_nodes(&user_ref);                       // 一跳邻居，以 NodeRef 返回
g.typed_out("employs", &user_ref);            // 谓词限定的遍历（0x09 面）
g.in_edges(&org_ref);                         // 谁指向这里
g.predicates();                               // 全部关系类型：一次 0x04 前缀扫描
g.by_attr_face(<BySince as KvIndex>::SLOT, &2020u16.to_be_bytes()); // 声明的属性面
g.unlink(1)?;                                 // 解码边体，精确删除它产出的那些面
```

分工：`Refs`/`Junction` 面向固定、强类型的端点对；图边面向开放端点、
运行期谓词、平行边与属性。节点的"类型"仍是普通文档 collection——ns 就是
类型标记，所以按节点种类过滤用的是那个 collection 自己的索引。

### 5. 跨表原子写：batch 路径

一个 `Batch`、一次提交，要么全活要么全无（ADR-0003）。单集合的常规写入
不需要调用方持有 batch：`put`、`purge`、`merge`、`replace` 自己建一个——主条目、
索引面、内嵌子文档、旧文档释放的引用、reduce 的折/回折、词表增长——一次提交，
所以一次语义写就是一次引擎提交、一个崩溃边界。下面这个调用方持有的 batch 是
给跨装配点的写入用的。文档写入用
`save_with_prev_into`，图与 junction 用各自的 `*_into` 兄弟：

```rust
let mut batch = Batch::default();
users.save_with_prev_into(&mut batch, &key, &new_row, Some(&old_row));
g.link_into(&mut batch, &head, &attrs, 7)?;
store.commit_batch(batch)?;                    // 行、面与账本一起落地
```

`save_with_prev_into` 是 `save_into`（只管编码）的语义兄弟：reduce 的
折/回折与旧面清理都经 `BatchStore`（batch 的读穿透视图）完成，所以同一批
里对同一组的两次 fold 看见的是本批已产出的值，不会重复计数
（[ADR-0031](docs/adr/0031-batch-write-semantics.md)）。subscribe 事件按裁
决留在 batch 路径之外：批量写者的事件归它自己，在未提交的批里发事件等于
宣布一次可能永不落地的写。调用方不手推 op 进批——`save_with_prev_into` /
`link_into` 替它们编码键与面；op 列表本身是引擎、wire 帧与 dynamic plan
消费的内部接缝。

### 6. 引擎后端

```toml
[dependencies]
okm-core = { version = "0.1", features = ["fjall"] }    # 或 "slatedb" / "redb"
```

- **fjall**（同步）：`FjallStore::open(path)` — 本地 LSM 引擎，单 `Database` 句柄，按需 `persist`。
- **slatedb**（异步）：`SlatedbStore::open(path, Arc<dyn ObjectStore>)` — 对象存储后端；构造 store 用 `slatedb::object_store` 的 re-export，版本永远和 slatedb 内部一致。异步遍历走 `AsyncJunction`。
- **TestStore**：测试用的引擎矩阵，跑的是真实引擎（slatedb 内存、fjall 临时目录、redb 临时文件）——没有会与真实实现漂移的 mock。

## 接口层级

四层，每层建在下一层之上。贯穿四层的规则是：**每层用下一层的货币说话**——声明层产出字节，
装配点调用引擎契约，引擎只看见字节。

| 层 | 接口 | 它承诺什么 |
|---|---|---|
| 声明层 | `KeyEncode` / `DocumentEncode` / `JunctionEncode` / `GraphEncode`（okm-derive） | 纯 `Vec<u8>` 进出——没有存储、没有 I/O、没有引擎类型；这正是每个 derive 保持单 item 纯函数的前提 |
| 装配层 | `Collection<S, K, R>` / `Junction<S, E>` / `Graph<S, E>`，加 reduce 与 subscribe 钩子 | 绑定**一个**引擎实例；读写都取 `&self`（写不需要 `&mut`：集合是 `!Sync`，单持有者在编译期就定死）；`Graph` 仍是 `&mut self`，它的谓词词表是可变的缓存字段；声明本身就是注册表，没有需要在运行期同步的 schema 对象 |
| 引擎契约 | `VirtualStorage` / `VirtualStorageAsync` / `SharedVirtualStorage`（`okm-core/src/engine/storage.rs`） | 编码字节上的七个方法——后端唯一必须说的一种语言 |
| 消费层 | okm-query / okm-stream / okm-graph / okm-vector / okm-ngram / bindings | 位于装配层之上，消费它的产物（有序扫描流、subscribe 事件、document），对 core 零改动 |

两种 schema 载体——编译期声明（okm-derive）与 schema 数据（okm-dynamic）——落到的都是下面
同样两层，所以「用哪种模式」从不改变引擎契约，也不改变字节。

### 引擎层的全貌

一套契约、两个世界、一个载体、两个适配器：

| 项 | 种类 | 契约 |
|---|---|---|
| `VirtualStorage` | 同步契约 | `put` / `get` / `del` / `scan_suffix` / `scan_range` / `scan_range_iter` / `commit_batch`；写操作取 `&self`（ADR-0026）；`scan_range` 是**唯一**的有序读原语——`scan_suffix` 与 `scan_range_iter` 都以它为默认实现 |
| `VirtualStorageAsync` | 异步镜像 | 同一套面，改为 await（`SlatedbStore`、`WireClient` 实现） |
| `SharedVirtualStorage` | 能力 | `shared_handle()` 返回**同一份物理键空间**的句柄，绝不是副本；`NestStorage` 与 `scan_range_iter` 的回表都依赖它 |
| `Batch` | 载体 | 有序 op 列表（`ops: Vec<(key, Option<value>)>`：`Some` 是 put、`None` 是 del，**不去重**，按序回放）。`Default` + `put` / `del` 就是它的全部 API，填它的是装配点；一次 `commit_batch` 作为一次引擎级写落盘 |
| `ScanIter` | 迭代器 | 覆盖各后端原生 owned 迭代器的封闭枚举（fjall / redb / slatedb / remote / buffered）；引擎能倒走的地方就保留 `DoubleEndedIterator` |
| `BatchStore<'a, S>` | 适配器 | 把 batch 当成写入器：点读先看本批待写项（倒序最新）再看 store，写只追加，扫描形态 panic（ADR-0031） |

同步契约共有八个实现——六个后端、两个适配器：

| 类型 | 位置 | 形态 | Shared |
|---|---|---|---|
| `FjallStore` | `engine/fjall_backend.rs` | 本地 LSM（feature `fjall`） | 是 |
| `RedbStore` | `engine/redb_backend.rs` | COW B-tree（feature `redb`） | 是 |
| `SlatedbSync` | `engine/slatedb_backend.rs` | slatedb 的同步视图；异步面是 `SlatedbStore` | 否 |
| `LocalStorageStore` | `engine/localstorage_backend.rs` | 浏览器 localStorage（feature `localstorage`） | 是 |
| `RemoteStore` | `engine/nest.rs` | 线上客户端；它的服务端半边 `NestStorage` 是一个引擎互斥量上的泵，**不是** `VirtualStorage` | 否 |
| `TestStore` | `engine/test_engine.rs` | 跑真实引擎的测试矩阵（slatedb 内存 / fjall / redb） | 是 |
| `EngineBox` | `bindings/okm-entry` | **适配器**：分派到进程内 `TestStore` 或宿主注入的 `Engine`（四个字节方法）——让宿主可以持有另一份 okm 构建 | 否 |
| `BatchStore` | `engine/storage.rs` | **适配器**：装饰一个 `S`，把写重定向进 `Batch`，自己什么都不落盘 | 否 |

#### `BatchStore` 是门面，不是后端

它实现它装饰的那个接口（`impl VirtualStorage for BatchStore<'_, S>`）——reduce 的
读-改-写因此能在不知道自己正往批里写的前提下跑完。它给的是重定向，不是能力：

- **写不穿透**——`put` / `del` 只追加进 `Batch`；**读穿透**——`get` 先看该键在批里
  最新的一条，再落 store；
- 自己不持有数据、只借用引擎（`store: &'a S`）：绑在借来的 batch 上的一次性视图，
  不能长期持有，也不能传出去；
- **提交在它外面**——`commit_batch` 是调用方对引擎的调用；在适配器上调用只会把内层 op
  并进外层批；
- 不实现 `SharedVirtualStorage`（所以 `NestStorage` 与 `scan_range_iter` 的回表都拒它），
  且所有扫描形态 panic，而不是给一份部分视图。

决策记录见 [docs/adr/](docs/adr/)；落地状态见 [docs/PLAN.md](docs/PLAN.md)。

## 设计要点

- **命名空间留在代码**——命名空间词汇表是编译期常量，永不落 KV。访问模式本身就在代码里（二进制 key、无分隔符、每段宽度由代码定义），ns 在代码里和整套 key 布局在代码里是同一件事。宏在编译期运行，那时没有 KV 可读——词汇表落 KV 是自举死锁。编号手动分配、append-only、永不复用；见 [ADR-0002](docs/adr/0002-namespace-dictionary.md)。
- **两套布局 regime**——主键定宽（零解析、热路径）；二级索引变长（判别文本放最前，UTF-8 字节序 = 字典序扫描；主键 ID 挟带在 key 末尾，value 留空）。定宽是**结构**的属性不是**数据**的属性；判据是访问模式：纯点查可 hash 成定宽，需要前缀/范围扫描必须保留原始文本。变长字段用长度前缀 `[len: u16][bytes]` 而非 NUL 结尾（无转义负担）；变长字段**之后**的定长字段失去编译期偏移、退到运行期游标——「定宽前缀 + 变长尾缀」保留大部分零解析收益。hex 稳定性测试对变长 key 照常生效：锁的是**编码方案本身**（前缀布局、长度字节端序、上限），不是具体字节。姓名→id 索引是主流情形，且几乎必然变长——索引存在就是为了回答前缀/范围查询。完整论证见 [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md)。
- **宏层刻意无存储**——encode/decode 是纯 `Vec<u8>` 进出函数；引擎选择与生命周期归装配处（`Collection::new(store)`）。这是每个 derive 保持单 item 纯函数的前提。
- **可移植性**：范式作用在字节层、与宿主语言无关——Python dataclass 写同样的 `encode()` 可复现布局（SlateDB 的 Python 绑定经 UniFFI 提供所需原语：`get` / `scan_prefix` + `KeyRange` / `WriteBatch` / 事务）。但可移植有结构性代价：类型错误从编译期移到运行时（`assert` 兜底而非编译器）、编码是字节拼接而非 memcpy 级指针偏移（热路径慢 1–2 个数量级）、装饰器/元类是运行期注册而非编译期展开。范式相同，保障级别由宿主语言能力决定。

## 为什么不用（现成的）数据库？

OKM 的潜在好处之一：选型烦恼消失了。实际选项还很多：PostgreSQL、DuckDB、Lakehouse、SurrealDB……OKM 说的是纯字节，任何能 put/get 字节的引擎都够格。
