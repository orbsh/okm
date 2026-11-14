# OKM — Object-Keyspace Mapping（对象键空间映射）

> And what, SQL, must we give in return?

[English version](README.md)

应用是数据的唯一读者，数据库就不该是一个网络服务。OKM 是一个嵌入式、类型安全、编译期优化的存储引擎——它没有查询语言、没有优化器、没有运行时计划，因为你的应用代码已经知道每个查询的形态，剩下的只是让编译器也知道。

```rust
use okm::prelude::*;

#[derive(KeyEncode)]                  // 身份独立声明：定宽、大端、无分隔符
pub struct UserKey {
    pub org_id: u32,
    pub user_id: u64,
}

#[derive(DocumentEncode)]             // 行：身份 + 载荷 + 访问方法
#[ok_ref(UserKey)]                    // 行自己钉住 key 类型
#[ok_ns(1)]                           // ns 声明在这里，不在组装处
#[ok_index(by_plan { fields(plan) })] // 一个面：条目键 = 被索引字段 + key 尾段
pub struct User {
    pub email: String,
    pub plan: Plan,
}
```

## SQL 是动态的，应用里的 SQL 是静态的

SQL 被设计成一种运行时才被解释的语言：查询是字符串，结构在运行前未知，于是数据库必须随身带着解析器、优化器、计划缓存——这些机制存在的唯一理由，就是"它不知道下一条查询长什么样"。

而你的应用不是这样。每一条查询都写在代码里，参数来自变量，形状在编译期就固定了：`WHERE plan = $1` 的形状永远是"按 plan 取行"，变的只是 `$1` 的值。所谓"动态查询"，绝大多数是**同一个静态形状换一个参数**；真正随用户输入改变结构的那一小部分——BI 面板里拼 WHERE、报表生成器、临时的 admin 查询——恰好是 SQL 的主场，也是嵌入式引擎不该假装能赢的地方，诚实的分界线就在这里。

这笔交易因此可以一句话说完：**把"查询在运行时才知道"这个前提从引擎里拿掉**。前提拿掉之后，随之消失的是一整串为它而生的机制：解析器、优化器、计划缓存、参数嗅探、prepared statement 的生命周期、连接池、以及为了绕开这一整套而存在的缓存层。它们不是被更好的实现替代，而是**失去了存在的理由**。

代价同样明确，也必须说明：查询集合封闭。schema 变更要重新编译；ad-hoc 分析要另走一条路（CDC 到 Delta Lake，让分析侧用任意引擎）；"随手连上 psql 看一眼"这件事没有了。静态化省下的不是某个常数，而是**一整套从未真正为你产出的能力**的维护税。

## 为什么不是 SQL + Redis？

现代后端的默认架构是“PostgreSQL 存真相，Redis 做加速”：

```
应用 ──网络 RTT──→ SQL    （解析 → 计划 → 执行）
应用 ──网络 RTT──→ Redis  （缓存，需要手工失效）
```

每一层都在为另一层的存在付钱：Redis 存在是因为 SQL 每次读取要付网络往返 + 解析 + 计划；SQL 中心化部署是因为查询是动态字符串，需要独立的解析服务。这两者互为因果，也是两者同时消失的原因。

| | SQL + Redis | OKM |
|---|---|---|
| 读一行 | 0.5–2 ms（RTT + 解析 + 计划） | ~1 µs（进程内 B-tree） |
| 缓存层 | 需要（读太慢） | 不存在（读取本身已是内存速度） |
| 缓存失效 | 一整门工程学科 | 没有缓存可失效 |
| 部署清单 | 应用 + 数据库服务 + 连接池 | 一个二进制 |
| 查询校验 | 运行时错误 | 编译期错误 |
| 业务边界 | 两个系统、无跨系统事务 | 一个类型域 |

Redis 的性能故事成立于“网络是给定前提”的世界。OKM 的统一论点是让这个前提消失——缓存层不是被替代，是被彻底抽掉。

## 两步语义：查询 = 集合运算 + 回表

所有查询归纳为两个原语：

```rust
// 第一步：键集合运算（纯内存，O(min(|A|,|B|))，基数大时自动最优）
let active: KeySet<UserId> = users.by_plan().contains(&[Plan::Pro]);
let paid: KeySet<UserId>   = users.by_email().prefix("team");

// 交集、并集、差集——都是结构化值，可序列化、可下推
let target = active.intersect(&paid);

// 第二步：回表（按主键批量取回完整行）
let rows: Vec<Users> = users.fetch(&target);
```

- **基数大不是优化问题，是一个 if 语句**：小集合建哈希、扫大集合，谁小是运行时事实，代码现塌判断，不需要优化器猜
- **键集合是可序列化的中间表示**：跨进程、跨网络、跨引擎传输时，传的是集合运算的结果而非原始数据
- **动态查询也有位置**：用户在 UI 里拼的条件过滤，形态天然是“谓词 → 键集合 → 回表”，动态性被限制在集合运算的输入端

## 面与引擎：声明时定形，装配时选引擎

索引不是独立对象，它是**行上的一个面**：`#[ok_index(...)]` 写在行声明里，条目键 = 被索引字段 + 这一行的 key 尾段。所以没有"另建一张索引表"这回事——面的形状跟着行一起被编译，键就是行的 key 类型；同一个 key 类型可以被多张行、多个面共用。

引擎选择发生在装配处，一次决定一个集合：

```rust
// 生产：持久化 LSM
let users: Collection<_, UserKey, User> = Collection::new(Fjall::open("data/users")?);

// 测试：纯内存，零 I/O
let users: Collection<_, UserKey, User> = Collection::new(Mem::default());

// 归档：同一种声明，另一个引擎实例
let archive: Collection<_, UserKey, User> = Collection::new(Fjall::open("data/archive")?);
```

所有引擎实现同一套行为契约（见 `engine_matrix_test`），换引擎不改应用逻辑——存储在哪是实现细节，位置不在类型里。

## 引擎之外的分层

诚实的世界，各走各的层：

- **Ad-hoc 分析 / BI** → CDC 淌业务日志持续导出 Delta Lake，分析侧任意引擎（DuckDB / Spark / Trino）
- **Schema 演进** → 模式变更是编译期事件：蓝绿两进程各跑各的版本，共享 COW 快照
- **动态模式**（租户自定义字段）→ okm-dynamic：结构是数据而不是类型，外层访问仍是静态优化路径

## 状态

内部多个项目生产使用。尚未承诺 API 稳定性，当前阶段适合评估和实验，不适合外部生产依赖。

## 优化器的角色收缩

这个洞对优化器的侵蚀最大。优化器存在的理由是“查询未知，引擎必须聪明”。静态世界里，任何优化器能做的事都可以被预先做成代码，且没有估计误差：

- 谓词下推 → 程序员先算集合再回表，天然下推
- join 顺序 → 集合运算的顺序由调用链决定，且程序员知道基数
- 索引选择 → 访问方法在声明 slot 时就绑定了
- cardinality 估计 → 不需要猜，写代码的人见过数据

这不是说“程序员永远比优化器聪明”，而是说：当查询集合封闭时，优化器的工作变成了一次性的开发期决策。

## 和历史上的先例对照

- **ORM 的查询集**：TypeORM/Diesel 的编译期查询构造器半承认了这一点，但最终仍编译成文本 SQL，把静态性浪费在最后一步——编译成字符串再交给动态引擎是双输
- **存储过程**：把逻辑移到数据侧，但语言封闭、无生态、调试地狱；OKM 把数据移到逻辑侧（嵌入式），保留了全部 Rust 生态
- **Sled/redb 等嵌入式 KV**：承认嵌入式优势，但停留在字节层，没有类型层
- **超编译（Futamura projections）**：理论上“把解释器特化成编译器”的第一层投影。静态查询 = 手动完成了这一次投影特化

## 综合体评估

| 特性 | 实现方式 | 设计意图 | 状态 |
|---|---|---|---|
| ORM 体验 | Rust 类型系统 + 派生宏 | 编译时生成存取代码，避免运行时解析 | ✅ 核心特性 |
| Redis 速度 | 精简优化的 KV 存储 | 嵌入式无网络开销，接近内存访问 | ✅ 目标性能 |
| PostgreSQL 持久性 | 业务日志 + 持久化存储 | 确保数据在系统故障后不丢失 | ✅ 设计目标 |
| 无边界的函数 | 嵌入式调用 Rust 函数 | 复杂逻辑直接用应用代码实现 | ✅ 独特优势 |
| 两步语义 | 集合运算与回表分离 | 理解匹配与物化分离 | ✅ 核心创新 |

**核心优势**：编译期类型安全（不匹配查询在编译时现形）、零成本抽象（编译后退化为纯指针偏移）、清晰模型（关系代数）与简洁 API。

**当前的挑战**：生态、生产验证、复杂查询（跨多个实体）、团队协作（社区为零）。

**适用性**：高（超 Redis 与传统的读写性能 + Rust 技术栈项目）；特定场景（快速构建概念验证）；不合适（现成生态和社区支持）。

## 与其他方案对比

| 方案 | 类型 | 核心优势 | 核心劣势 | 适用场景 |
|---|---|---|---|---|
| OKM | 嵌入式 Rust ORM | 性能潜力、扩展性、类型安全 | 生态空白 | 性能敏感、Rust 技术探索 |
| Diesel | Rust ORM | 成熟稳定、编译期检查 | 异步受限、学习曲线陡 | 传统 Web 后端 |
| SQLx | 异步 SQL 工具包 | 异步优先、编译时检查、灵活 | 非 ORM，需手动处理关系 | 异步服务、微服务 |
| SeaORM | 动态 ORM | 灵活、动态、功能丰富 | 抽象层带来性能开销 | 快速开发 REST/GraphQL |
| rusqlite | SQLite 绑定 | 轻量、无依赖、简单 | 同步 API、SQLite 限制 | CLI 工具、桌面应用 |
| Redis | 内存数据库 | 极快、数据结构丰富 | 不持久、需网络访问 | 缓存、会话存储、实时分析 |

## 总结与建议

OKM 代表了一种反传统数据库设计思路：把关系数据库的查询模式映射到 KV 存储结构，以 ORM 式的类型定义和编译时生成代码替代运行时查询。

值得警惕的是本项目的代价：静态化不是免费的——运行时的自由度（BI 工具、临时查询、动态报表）被真实放弃了。这决定了 OKM 的定位是应用数据库，不是分析数据库，Ad-hoc 是 SQL 真正的主场。

对于绝大多数项目，引入 OKM 目前不划算：它的生态是零，所有组件都要自建。它更适合作为技术探索和实验平台，验证其设计理念；如有嵌入式的需求，rusqlite（SQLite）是更稳妥的选择；若追求高性能，Redis（配合持久化）仍是主流方案；若追求成熟，SeaORM 或 Diesel 更合适。

OKM 的价值在于其前期的设计思路，特别是“无边界函数”和“两步语义”，可为未来数据库设计提供新范式。

## 使用方法

### 1. 声明：key、行与关系

完整的声明词汇（`KeyEncode` / `DocumentEncode` / `GraphEncode`，以及 `#[ok_index]` 的
`fields`/`includes`/`key`/`func`/`where` 注解）见[建模指南](docs/MODELING.zh-CN.md)「声明基础」。
三种形态：**身份**、**行**、**图**。多对多不再有独立载体——它是一张普通 collection（键就是那对身份）加一条 `#[ok_lead]` 反向面。

```rust
#[derive(KeyEncode)]                        // 身份：定宽字段、大端、无分隔符
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(DocumentEncode)]                   // 行：身份 + 载荷 + 访问方法
#[ok_ref(UserKey)]                          // 行自己钉住 key 类型
#[ok_ns(1)]                                 // ns 声明在这里，不在组装处
#[ok_index(by_org { fields(org_id) })]      // 一个面：条目键 = 被索引字段 + key 尾段
pub struct User { pub org_id: u32, pub bio_len: u32 }

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
let row: Option<User> = users.get_row(&key);

// 按主键前缀读：键本身就是访问路径。合成键天生分组，"这个 org 的全部用户"
// 不需要索引、不需要声明查询——探针按声明序命名前导字段。
let org_users = users.scan_keys(&UserKey::prefix().org_id(7).finish());
// 惰性：`.take(n)` 是 LIMIT，`.skip(n)` 翻页，`.rev()` 反向；每条是 `Result<UserKey>`
// （只给行——要文档就按局部读一次）。

// 按访问方法读：维度不在键里，所以它是一条声明出来的面——最左前缀扫描 + 回表。
// 探针与主键那套对称：`ByOrg::prefix()` 按该索引声明的 `fields(...)` 顺序给字段，
// `finish()` 得到的就是手拼字节会得到的那串。
for (pk, row) in users.scan::<ByOrg>(&ByOrg::prefix().org_id(7).finish()) {
    // pk.decoded: UserKey，row: Option<User>
}

// 索引首字段上的比较就是一段物理键区间——字节序即值序，
// 所以这里只读 [1, 100) 内的行。
users.scan_range::<ByOrg>(&1u32.to_be_bytes(), Some(&100u32.to_be_bytes()));

// 删：主表 + 恰是这份文档产出的那些面。
users.purge(&key);   // 主条目 + 面 + 动态段，一次提交；删除集合派生自存储行
```

三道读门就是 SQL 藏起来的那个建模决策：谓词落在前导键段上，就是对主表的一次前缀扫描（不建索引、不做声明）；其余维度都是声明出来的面；单行就是点查。这里的索引不是"优化器也许会用"的第二份副本——前缀读需要的顺序已经是存储顺序，所以标识一行的键同时也在给它分组。

前缀读不如 SQL 谓词抽象，也不如它灵活。这就是取舍，值得直说。SQL 谓词是一份"优化器会找到访问路径"的承诺；很多引擎并不兑现它，而它们毁约的方式比报错更糟——凡索引与键顺序相关之处（MySQL 的复合索引是典型），左前缀规则把物理布局变成查询的一部分：`INDEX(a, b, c)` 能不能服务 `WHERE b = 1`，取决于建表时选的字段顺序。这是抽象泄漏，不是抽象，代价既在灵活性上，也在直觉上。OKM 走另一条分岔：没有优化器，因此没有会被打破的承诺——键的布局就是读路径，前缀就是存储序上的一段区间；代价是访问模式在建模期定下来，而不是每条查询各定一次。

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

三道门，区别在「这次调用能看到什么」，不在方便与否：

| 门 | 看到 | 付出 |
|---|---|---|
| `get` / `put`（载荷 `&R`） | 只有声明行（`R`），永远不碰动态段 | 不载入字段名词汇表，不读段——编译期 schema 这条路 |
| `get::<(parts::Row, parts::Value)>` | 恰好是点名的那些槽 | 只付点到的槽 |
| `put`（载荷是 map） | 整份文档：两半 | 只读这次点到的那些臂 |

写在两侧都只有一个动词，走哪条路由**载荷**决定——调用方不必先把自己的数据翻译成一个动词名：

| 载荷 | 路径 |
|---|---|
| `&R`（只有 typed 侧） | 行与它的面；新行快速路径，所以**不**清理旧值的条目 |
| `&Dict` / `&Variant::Map(map)` | 局部文档：声明名是一次行 RMW（旧面被扫掉、账本被补偿），其余名字覆盖动态段里对应的那个名字，map 没点到的名字保持库里的值 |
| `(&Dict,)` / `(&Variant::Map(map),)` | 整份文档：map 就是文档——缺席的声明字段取默认，未点名的动态名被移除 |
| `purge` | 两槽——两侧唯一的删除动词 |

这几行里的 map 就是 `okm_value::Dict`：一个按插入序的名字 → 值 map，也是「文档对象」的唯一写法（`Variant::Map` 携带的就是它；插入序即是契约——ADR-0063）。

载荷是调用方的数据，所以写是可失败的（`okm_core::Error`）：声明名带着错误的 kind 是
`Error::KindMismatch`，绝不静默取默认，并且什么都不写。缺席的那一半不花钱：
schema 决定这次计划走哪个臂——没有声明名的 map 不读行，没有动态名的 map 不读段、也不载入词汇表。

读是局部，拼装归调用方：点读把每个槽各答各的，行转成一个值（`Document::to_value`），
两个值做合并（`Variant::merge` / `merged`——对象按名字逐个合并、嵌套对象递归，
其余情况右侧替换左侧）。这里**刻意不提供**拼装函数：两个原语就是全部，组合是调用方的两行代码。

一套编码服务声明行与外部数据。未知字段名是正常输入：在每表的字段名词汇表里分配编号，落进动态段（`[id][type][len][value]` 帧、嵌套对象递归）。声明字段保持 typed 且可索引。

```rust
// 读：按局部点名。Row 是槽 0，Value 是槽 1（对象形态）；各自是独立的 Option，
// 所以只有动态段的文档答 (None, Some(obj))。
let (row, dyn_seg) = users.get::<(parts::Row, parts::Value)>(&key)?;

// 拼装文档：行转成一个值，动态段叠在它上面。只有两个槽都空才是 None——
// 只有动态段的文档照样答得出来。
let doc = match (row, dyn_seg) {
    (None, None) => None,
    (row, dyn_seg) => Some(
        row.map(|r| r.to_value())
            .unwrap_or_default()
            .merged(dyn_seg.unwrap_or_default()),
    ),
};

// 写：一个动词，载荷选路。
// merge——只动点名的名字；命中声明的名字走 typed 路径（面、reduce、事件一并生效），
// 未知名字并入动态段。
users.put(&key, &patch)?;                       // Dict

// replace——map 就是文档（缺席的声明字段取默认，未点名的动态名被移除）。
// 单元素元组就是「整份文档」那个臂。
users.put(&key, (&doc.unwrap_or_default(),))?;

// 已经拿在手上的值直接当载荷写；非对象载荷被拒绝——标量是字段的值，不是文档。
users.put(&key, &Variant::Map(patch))?;

// purge：两槽一起清，一次提交。
users.purge(&key)?;
```

嵌入式语言读取器（Python / Steel 摊位）经 schema 导出消费动态 codec——`bindings/okm-python`（PyO3）与 `bindings/okm-steel` 与 Rust derive 字节一致；版本默认值迁移在该路径同样生效。

### 6. 引擎后端

```toml
[dependencies]
okm-core = { version = "0.1", features = ["fjall"] }    # 或 "redb"；异步引擎在 okm-async
```

- **fjall**（同步）：`FjallStore::open(path)` — 本地 LSM 引擎，单 `Database` 句柄，按需 `persist`。
- **slatedb**（异步）：`SlatedbStore::open(path, Arc<dyn ObjectStore>)` — 对象存储后端；构造 store 用 `slatedb::object_store` 的 re-export，版本永远和 slatedb 内部一致。异步遍历走对应的 async 迭代器。
- **TestStore**：测试用的引擎矩阵，跑的是真实引擎（slatedb 内存、fjall 临时目录、redb 临时文件）——没有会与真实实现漂移的 mock。

### 4. 运行时：关系（图）

一张图的声明就是它的全部：`#[ok_ns]` 定信任域，`#[ok_nodes(User, Org)]` 列出成员**表**（派生宏据此生成枚举与 `NodeSet`，一个变体一个集合），`#[ok_index]` 是显式声明的面。

```rust
#[derive(GraphEncode, Clone, Default)]
#[ok_ns(100)]
#[ok_nodes(User, Org)]
#[ok_index(by_since { fields(since_year) })]
struct Employment {
    since_year: u16,
    weight: u32,
}
```

写只有一个动词，碎片（`Fragment`）是它唯一的输入：声明里收节点与边，链上可选的簇与三个 reduce，`None` 的部分就是不做。

```rust
let mut x = Fragment::<Employment>::new([
    Decl::Node(EmploymentNode::User, user_key),          // 端点按需创建
    Decl::Edge(EdgeHead { src, dst, predicate: "employs".into() }),
]);
x.cluster(Cluster::new(Kind::EVENT, payload))            // 成簇（可选）
 .reduce_node::<User>(|u| u.seen += 1)                   // 更新走 reduce
 .reduce_edge(|e| e.weight += 1)
 .reduce_cluster(|c| c.insert("weight", …));
graph.incorporate(x)?;                                   // 一批落地：端点、边、成员行、reduce
```

删除是反向动词，两条规则不同：删节点连带删它的所有边（一条边不能缺一端），删边保留两端（之后它们可能不再连任何边——"节点只在成为端点时存在"是写入前提，不是删除后的不变量）。

```rust
graph.retract(Fragment::new([]).delete_node(user_key).delete_edge(7))?;
```

遍历是有序流，一跳一次 `next()`：`out` 与 `inn` 产出同一个 `EdgeStep`（方向是参数不是两套 API），谓词过滤下推到 `0x08`/`0x09` 前缀，`find`/`take_while` 天然短路，drop 即停。流实现 `Seekable`，于是 `intersect`/`difference`/`union` 与集合运算、簇成员流共用同一套实现。

簇（超边）落在三张固定面：`0x0A` 簇行（值 = `parent_id` + 动态载荷）、`0x0B` 边成员、`0x0C` 子簇索引；加 `0x0D` 边→簇的派生索引，于是"某条边属于哪些簇"是一次点查。成员只记边，参与者是成员边的两端，投影在调用方做。

### 5. 跨表原子写：一个碎片，一次提交

`Fragment` 就是跨表写的载体：同一批里的节点、边、成员行、reduce 一起提交或一起不提交。没有跨表事务语言、也没有级联机制——一致性靠"一个碎片 = 一次提交"，以及写入口强制的两条前提（节点必须是某条边的端点，簇至少两个成员）。

```rust
let mut x = Fragment::<Employment>::new([Decl::edge_at(head, 7)]);  // 同一 id 重述 = 就地更新
x.reduce_edge(|e| *e = new_payload);
graph.incorporate(x)?;                                              // id、面、成员行都不动
```

不同载体是各自的一次提交：一条边的载荷在图上，它的开放属性在文档的动态段（`0x01`），两者属于两个写路径；需要两者同批时，由调用方把它们放进同一个上层流程，而不是让引擎去猜。

## 接口层级

四层，每层建在下一层之上。贯穿四层的规则是：**每层用下一层的货币说话**——声明层产出字节，
装配点调用引擎契约，引擎只看见字节。

| 层 | 接口 | 它承诺什么 |
|---|---|---|
| 声明层 | `KeyEncode` / `DocumentEncode` / `GraphEncode`（okm-derive） | 纯 `Vec<u8>` 进出——没有存储、没有 I/O、没有引擎类型；这正是每个 derive 保持单 item 纯函数的前提 |
| 装配层 | `Collection<S, K, R>` / `Graph<S, E>`，加 reduce 与 subscribe 钩子 | 绑定**一个**引擎实例；读写都取 `&self`（写不需要 `&mut`：集合是 `!Sync`，单持有者在编译期就定死）；`Graph` 仍是 `&mut self`，它的谓词词表是可变的缓存字段；声明本身就是注册表，没有需要在运行期同步的 schema 对象 |
| 引擎契约 | `VirtualStorage` / `SharedVirtualStorage`（`okm-core/src/engine/storage.rs`）；`VirtualStorageAsync` 是它在 `okm-async` 里的镜像（ADR-0059） | 编码字节上的七个方法——后端唯一必须说的一种语言；异步镜像只服务于「自身 API 就是异步」的引擎与传输 |
| 消费层 | okm-query / okm-stream / okm-graph / okm-vector / okm-ngram / bindings | 位于装配层之上，消费它的产物（有序扫描流、subscribe 事件、document），对 core 零改动 |

两种 schema 载体——编译期声明（okm-derive）与 schema 数据（okm-dynamic）——落到的都是下面
同样两层，所以「用哪种模式」从不改变引擎契约，也不改变字节。

### 引擎层的全貌

一套契约、两个世界、一个载体、两个适配器：

| 项 | 种类 | 契约 |
|---|---|---|
| `VirtualStorage` | 同步契约 | `put` / `get` / `del` / `scan_suffix` / `scan_range` / `commit_batch`；写操作取 `&self`（ADR-0026）；`scan_range` 是**唯一**的有序读原语（惰性 `ScanIter`，逐条 `(key, value)`）——`scan_suffix` 以它为默认实现，要整批就自己 `.collect()` |
| `VirtualStorageAsync` | 异步镜像 | 同一套面，改为 await（`SlatedbStore`、`WireClient` 实现） |
| `SharedVirtualStorage` | 能力 | `shared_handle()` 返回**同一份物理键空间**的句柄，绝不是副本；`NestStorage` 与 `scan_range` 的回表都依赖它 |
| `Batch` | 载体 | 有序 op 列表（`ops: Vec<(key, Option<value>)>`：`Some` 是 put、`None` 是 del，**不去重**，按序回放）。`Default` + `put` / `del` 就是它的全部 API，填它的是装配点；一次 `commit_batch` 作为一次引擎级写落盘 |
| `ScanIter` | 迭代器 | 覆盖各后端原生 owned 迭代器的封闭枚举（fjall / redb / slatedb / remote / buffered）；引擎能倒走的地方就保留 `DoubleEndedIterator` |
| `BatchStore<'a, S>` | 适配器 | 把 batch 当成写入器：点读先看本批待写项（倒序最新）再看 store，写只追加，扫描形态 panic（ADR-0031） |

同步契约共有八个实现——六个后端、两个适配器：

| 类型 | 位置 | 形态 | Shared |
|---|---|---|---|
| `FjallStore` | `engine/fjall_backend.rs` | 本地 LSM（feature `fjall`） | 是 |
| `RedbStore` | `engine/redb_backend.rs` | COW B-tree（feature `redb`） | 是 |
| `SlatedbSync` | `engine/slatedb_backend.rs` | slatedb 的同步视图；异步面是 `SlatedbStore` | 否 |
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
- 不实现 `SharedVirtualStorage`（所以 `NestStorage` 与 `scan_range` 的回表都拒它），
  且所有扫描形态 panic，而不是给一份部分视图。

决策记录见 [docs/adr/](docs/adr/)；落地状态见 [docs/PLAN.md](docs/PLAN.md)。

## 设计要点

- **命名空间留在代码**——命名空间词汇表是编译期常量，永不落 KV。访问模式本身就在代码里（二进制 key、无分隔符、每段宽度由代码定义），ns 在代码里和整套 key 布局在代码里是同一件事。宏在编译期运行，那时没有 KV 可读——词汇表落 KV 是自举死锁。编号手动分配、append-only、永不复用；见 [ADR-0002](docs/adr/0002-namespace-dictionary.md)。
- **两套布局 regime**——主键定宽（零解析、热路径）；二级索引变长（判别文本放最前，UTF-8 字节序 = 字典序扫描；主键 ID 挟带在 key 末尾，value 留空）。定宽是**结构**的属性不是**数据**的属性；判据是访问模式：纯点查可 hash 成定宽，需要前缀/范围扫描必须保留原始文本。变长字段用长度前缀 `[len: u16][bytes]` 而非 NUL 结尾（无转义负担）；变长字段**之后**的定长字段失去编译期偏移、退到运行期游标——「定宽前缀 + 变长尾缀」保留大部分零解析收益。hex 稳定性测试对变长 key 照常生效：锁的是**编码方案本身**（前缀布局、长度字节端序、上限），不是具体字节。姓名→id 索引是主流情形，且几乎必然变长——索引存在就是为了回答前缀/范围查询。完整论证见 [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md)。
- **宏层刻意无存储**——encode/decode 是纯 `Vec<u8>` 进出函数；引擎选择与生命周期归装配处（`Collection::new(store)`）。这是每个 derive 保持单 item 纯函数的前提。
- **可移植性**：范式作用在字节层、与宿主语言无关——Python dataclass 写同样的 `encode()` 可复现布局（SlateDB 的 Python 绑定经 UniFFI 提供所需原语：`get` / `scan_prefix` + `KeyRange` / `WriteBatch` / 事务）。但可移植有结构性代价：类型错误从编译期移到运行时（`assert` 兜底而非编译器）、编码是字节拼接而非 memcpy 级指针偏移（热路径慢 1–2 个数量级）、装饰器/元类是运行期注册而非编译期展开。范式相同，保障级别由宿主语言能力决定。


## 相关文档

- [Modeling Guide](docs/MODELING.zh-CN.md) — 规范化的建模方法（四层、三条读路径、covering index 克制）
- [Architecture](docs/ARCHITECTURE.zh-CN.md) — 两层声明机器（派生宏展开 + 常量表）
- [Key Layout](docs/internals/key-layout.zh-CN.md) — 键空间布局与固定面分配
- [Query Recipes](docs/query-recipes.zh-CN.md) — merge / intersect / difference / union 如何拼成 SQL 形状
