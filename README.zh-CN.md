# OKM — Object-Keyspace Mapping（对象键空间映射）

> ORM 的体验，Redis 的速度，PostgreSQL 的持久，不受边界限制的功能。

> 英文版为主文档（[README.md](README.md)），本文为对应中文版。

OKM 是对标 ORM 的范式——ORM 将对象映射到关系表，OKM 将对象映射到 KV 键空间。通过派生宏 `#[derive(KeyEncode)]` / `#[derive(EdgeEncode)]` + 数字命名空间 ID，构建零成本抽象语义数据层：开发侧如同 ORM 般声明式，编译后退化为纯指针偏移计算。

关联阅读：[KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) — 底层架构与设计模式（编码原理、索引策略、引擎层取舍）；[建模指南](docs/MODELING.zh-CN.md) — 规范性 schema 建模方法（四层建模法、访问方法强制、覆盖索引克制、主键复合边界）。

## 为什么：代码即 DDL

SQL 的核心价值不是执行性能，而是关系模型交付的可读性、建模规整度和团队协作确定性。裸露的二进制 Key 会退化为面条代码——团队无法理解 Key 的排列规则。解决方案：用 Rust 类型系统替代 SQL DDL，把 Schema 正确性从运行时数据库引擎提到编译期编译器。

- **Struct 即 DDL**：SQL 定义表结构，Rust Struct 定义 Key 编码。编译器保证格式一致——任何试图写入错误格式 Key 的代码在编译期直接报错，不需要运行时校验。
- **无模式是陷阱**：无模式存储来者不拒（数字字段可以写成字符串 `"25"` 甚至数组 `[25]`），代价是所有消费端代码写满防御性解析。强类型把脏数据在 `cargo check` 阶段熔断——连生成的资格都没有——同时保留 KV 引擎的硬件级速度，免掉 PG 的运行时 DDL 锁开销和 SQL 字符串解析税。
- **Hex 稳定性测试**：编码漂移的最终防线。硬编码历史 hex 字节锁定物理 Key 布局；任何改动 ns、字段顺序或宽度的行为都让 CI 立刻失败（见 `okm/tests/integration.rs`）。

判定：SQL 的核心价值是面向人类的结构化纪律。KV 只要贯彻「代码即 DDL 的强类型编码 + 多版本 Enum 懒迁移 + 双写 Key 指针契约 + hex 硬编码单元测试」，就同时获得了：编译期 Schema 安全（Rust 编译器）+ 运行时极致性能（LSM-Tree）+ 零停机演进（版本化 Enum）+ 团队可维护性（Struct 注释即文档）+ 编码漂移防护（hex 单元测试）。在 Schema 安全性上完成对 SurrealDB（无模式）和 PostgreSQL（运行时 DDL 锁）的双向反超。

## 物理收益

- **85% 前缀压缩**：14 字节字符串前缀 → 2 字节数字命名空间。
- **100% 定长 Key**（全量身份）：所有字段的绝对字节偏移被编译期死锁，反序列化纯指针切片，零解析。
- **32768 个命名空间**：u16 的最高位 niche 为边方向位（见 [ADR-0001](docs/adr/0001-direction-bit-niche.md)）。
- **缓存局部性**：LSM-Tree 布隆过滤器拦截 + 内存 Seek，热路径 CPU 缓存局部性极好。
- **零运行时开销**：无正则/split/AST——查询路径随 `cargo build --release` 固化为机器码。

## 状态

已实现：

- `KeyEncode` — 定宽 key 编码（`u32` / `u64` / `[u8; N]`），大端序，编译期 `KEY_LEN` / `FIELD_WIDTHS`，`encode_prefix_named` 截断原语。
- `EdgeEncode` — 双向边，各端点身份宽度可独立声明（`#[kv_head(...)]`），2 字节方向位头部，查询方法生成在端点类型上。
- `EdgeTable<S, E>` — 组装点：引擎 + 边类型 = 一条关系的操作面（`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`）。
- 引擎后端走 Cargo feature：`fjall`（同步 `FjallStore`）、`slatedb`（异步 `SlatedbStore` + `AsyncCollection`），测试用内存 `MockStore`。

路线图（设计已定，尚未实现——[ADR-0006](docs/adr/0006-row-node-model.md)、[ADR-0004](docs/adr/0004-value-side-and-wrappers.md)、[ADR-0005](docs/adr/0005-secondary-index-slots.md)）：

- `RowEncode` — 单宏声明行（Node）：`#[kv_ref]` 身份 + 载荷字段 + `#[kv_index(...)]` 访问方法；`ValueEncode` 宏并入其中（版本化 payload、TLV 扩展区、字段 wrapper 作为编码规则保留）。
- 字段级编码 wrapper（`Enum<T>`、`Offset<T>`、`Delta<T>`、`VarInt<T>`、`Reverse<T>` …）。
- 二级索引（访问方法）——**行 struct** 上的 `#[kv_index(name { fields(…), includes(…), key(…) })]`：对 **payload 字段**（按声明序）建组合索引；无 per-index slot/ns——2 字节表命名空间已区分所有 entry；最左前缀扫描；`key(…)` 把 key 尾部携带的主键截断到命名子集（`encode_prefix_named`），默认取满主键；`includes` 覆盖索引定位为高扇出查询的物化视图。
- `Table<S, K, R>` 行装配点与边 `EdgeTable` 并列；变长载荷/索引字段（`String`），key 保持定宽。
- 多引擎混用——同一进程内不同 ns 段可绑不同引擎（交易走 fjall、日志走 slatedb）；原子性止于单引擎内，ns 编号全库唯一。
- 快照导出——行 → Parquet，与引擎无关（备份 / 数据交换 / lakehouse 分析）；ns 还原为描述性文本，列名即字段名。

## 使用方法

### 1. 定义端点 key 与边（声明）

完整的声明词汇（`KeyEncode` / `EdgeEncode` / `RowEncode`、`#[kv_index]` 的 `fields`/`includes`/`key` 注解）见[建模指南](docs/MODELING.zh-CN.md)「声明基础」。摘要：

```rust
#[derive(KeyEncode)] #[kv_ns(1)]
pub struct UserKey { pub org_id: u32, pub user_id: u64 }

#[derive(EdgeEncode)] #[kv_ns(4)]
pub struct UserToSessionEdge {
    #[kv_head(org_id, user_id)]
    pub user_id: UserKey,
    pub session_id: SessionKey,
}
```

### 2. 运行时：连边与读写（速览）

完整用法（含反向查询、截断身份、扫描回表、Schema 稳定性测试）见[建模指南](docs/MODELING.zh-CN.md)「声明基础」之后的运行时小节。

```rust
// 边：原子双写 + 双向查询
let mut edges: EdgeTable<_, UserToSessionEdge> = EdgeTable::new(store);
edges.link(&user, &s1);
let sessions = user.get_session(&edges);

// 行：写主键 + 全部索引条目；按访问方法扫描
let mut t = <User as Row>::table(store, 9);
t.put(&user, &user_row);
let rows = t.scan::<ByOrg>(&7u32.to_be_bytes());
```

`scan::<ByOrg>` 的 `ByOrg` 来自索引名：`kv_index(by_org ...)` 生成类型 `__OkmIndex_User_by_org`（机械拼接，无大小写转换），`use __OkmIndex_User_by_org as ByOrg` 后即可用短名。声明怎么写见[建模指南](docs/MODELING.zh-CN.md)「声明基础」。

### 3. 引擎后端

```toml
[dependencies]
okm = { version = "0.1", features = ["fjall"] }    # 或 "slatedb"
```

- **fjall**（同步）：`FjallStore::open(path)` — 本地 LSM 引擎，单 `Database` 句柄，按需 `persist`。
- **slatedb**（异步）：`SlatedbStore::open(path, Arc<dyn ObjectStore>)` — 对象存储后端；构造 store 用 `slatedb::object_store` 的 re-export，版本永远和 slatedb 内部一致。异步遍历走 `AsyncCollection`。
- **MockStore**：内存 `BTreeMap`，memcmp 序——与真实引擎迭代语义一致，测试套件使用。

## 项目结构

```
okm-derive/        过程宏 crate：KeyEncode、RowEncode、EdgeEncode（零 I/O）
okm/src/key.rs     KeyEncode trait + PrefixKey
okm/src/index.rs   Row + KvIndex trait + 索引扫描辅助
okm/src/edge.rs    KvEdge trait + 方向位头部
okm/src/storage.rs  VirtualStorage trait + MockStore
okm/src/table.rs       Table<S, K, R> 行装配点
okm/src/collection.rs  EdgeTable<S, E> 边装配点
okm/src/fjall_backend.rs    fjall 适配（feature "fjall"）
okm/src/slatedb_backend.rs  slatedb 适配（feature "slatedb"）
okm-query/         扩展算子 crate：merge_join、group_by（消费 scan 有序流，零 core 依赖）
okm/tests/         integration（MockStore）、fjall_eval、slatedb_eval
docs/adr/          架构决策记录（docs/PLAN.md 为实施计划）
```

## 设计要点

- **命名空间留在代码**——命名空间字典是编译期常量，永不落 KV。访问模式本身就在代码里（二进制 key、无分隔符、每段宽度由代码定义），ns 在代码里和整套 key 布局在代码里是同一件事。宏在编译期运行，那时没有 KV 可读——字典落 KV 是自举死锁。编号手动分配、append-only、永不复用；见 [ADR-0002](docs/adr/0002-namespace-dictionary.md)。
- **两套布局 regime**——主键定宽（零解析、热路径）；二级索引变长（判别文本放最前，UTF-8 字节序 = 字典序扫描；主键 ID 挟带在 key 末尾，value 留空）。定宽是**结构**的属性不是**数据**的属性；判据是访问模式：纯点查可 hash 成定宽，需要前缀/范围扫描必须保留原始文本。变长字段用长度前缀 `[len: u16][bytes]` 而非 NUL 结尾（无转义负担）；变长字段**之后**的定长字段失去编译期偏移、退到运行期游标——「定宽前缀 + 变长尾缀」保留大部分零解析收益。hex 稳定性测试对变长 key 照常生效：锁的是**编码方案本身**（前缀布局、长度字节端序、上限），不是具体字节。姓名→id 索引是主流情形，且几乎必然变长——索引存在就是为了回答前缀/范围查询。完整论证见 [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md)。
- **宏层刻意无存储**——encode/decode 是纯 `Vec<u8>` 进出函数；引擎选择与生命周期归装配处（`Collection::new(store)`）。这是每个 derive 保持单 item 纯函数的前提。
- **可移植性**：范式作用在字节层、与宿主语言无关——Python dataclass 写同样的 `encode()` 可复现布局（SlateDB 的 Python 绑定经 UniFFI 提供所需原语：`get` / `scan_prefix` + `KeyRange` / `WriteBatch` / 事务）。但可移植有结构性代价：类型错误从编译期移到运行时（`assert` 兜底而非编译器）、编码是字节拼接而非 memcpy 级指针偏移（热路径慢 1–2 个数量级）、装饰器/元类是运行期注册而非编译期展开。范式相同，保障级别由宿主语言能力决定。

## 为什么不用（现成的）数据库？

OKM 的潜在好处之一：选型烦恼消失了。实际选项还很多：PostgreSQL、DuckDB、Lakehouse、SurrealDB……OKM 说的是纯字节，任何能 put/get 字节的引擎都够格。
