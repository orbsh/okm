# OKM — Object-Keyspace Mapping（对象键空间映射）

> 英文版为主文档（[README.md](README.md)），本文为对应中文版。

OKM 是对标 ORM 的范式——ORM 将对象映射到关系表，OKM 将对象映射到 KV 键空间。通过派生宏 `#[derive(KeyEncode)]` / `#[derive(EdgeEncode)]` + 数字命名空间 ID，构建零成本抽象语义数据层：开发侧如同 ORM 般声明式，编译后退化为纯指针偏移计算。

关联阅读：[KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md) — 底层架构与设计模式（编码原理、索引策略、引擎层取舍）。

## 为什么：代码即 DDL

SQL 的核心价值不是执行性能，而是关系模型交付的可读性、建模规整度和团队协作确定性。裸露的二进制 Key 会退化为面条代码——团队无法理解 Key 的排列规则。解决方案：用 Rust 类型系统替代 SQL DDL，把 Schema 正确性从运行时数据库引擎提到编译期编译器。

- **Struct 即 DDL**：SQL 定义表结构，Rust Struct 定义 Key 编码。编译器保证格式一致——任何试图写入错误格式 Key 的代码在编译期直接报错，不需要运行时校验。
- **无模式是陷阱**：无模式存储来者不拒（数字字段可以写成字符串 `"25"` 甚至数组 `[25]`），代价是所有消费端代码写满防御性解析。强类型把脏数据在 `cargo check` 阶段熔断——连生成的资格都没有——同时保留 KV 引擎的硬件级速度，免掉 PG 的运行时 DDL 锁开销和 SQL 字符串解析税。
- **Hex 稳定性测试**：编码漂移的最终防线。硬编码历史 hex 字节锁定物理 Key 布局；任何改动 ns、字段顺序或宽度的行为都让 CI 立刻失败（见 `okm/tests/integration.rs`）。

## 物理收益

- **85% 前缀压缩**：14 字节字符串前缀 → 2 字节数字命名空间。
- **100% 定长 Key**（全量身份）：所有字段的绝对字节偏移被编译期死锁，反序列化纯指针切片，零解析。
- **32768 个命名空间**：u16 的最高位 niche 为边方向位（见 [ADR-0001](docs/adr/0001-direction-bit-niche.md)）。
- **零运行时开销**：无正则/split/AST——查询路径随 `cargo build --release` 固化为机器码。

## 状态

已实现：

- `KeyEncode` — 定宽 key 编码（`u32` / `u64` / `[u8; N]`），大端序，编译期 `KEY_LEN` / `FIELD_WIDTHS`，`encode_prefix_named` 截断原语。
- `EdgeEncode` — 双向边，各端点身份宽度可独立声明（`#[kv_head(...)]`），2 字节方向位头部，查询方法生成在端点类型上。
- `Collection<S, E>` — 组装点：引擎 + 边类型 = 一条关系的操作面（`link` / `unlink` / `forward` / `reverse` / `reverse_prefix`）。
- 引擎后端走 Cargo feature：`fjall`（同步 `FjallStore`）、`slatedb`（异步 `SlatedbStore` + `AsyncCollection`），测试用内存 `MockStore`。

路线图（设计已定，尚未实现——[ADR-0004](docs/adr/0004-value-side-and-wrappers.md)）：

- `ValueEncode` — 版本化 value payload（懒迁移）与 TLV 扩展区。
- 字段级编码 wrapper（`Enum<T>`、`Offset<T>`、`Delta<T>`、`VarInt<T>`、`Reverse<T>` …）。
- 变长 key 字段（`String` 带 `[len: u16]` 前缀），用于二级索引。

## 使用方法

### 1. 定义端点 key

```rust
use okm::{EdgeEncode, KeyEncode};

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

### 2. 声明一条边

```rust
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

`#[kv_head(field, ...)]` 声明该端点在**这条边里**哪些字段算身份；不标注 = 全量 key 即身份。字段名必须是端点声明序的前缀（宏生成的编译期检查）。

### 3. 连接、断开、查询

```rust
use okm::Collection;

let store = okm::MockStore::default(); // 或 FjallStore / SlatedbStore
let mut edges: Collection<_, UserToSessionEdge> = Collection::new(store);

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

### 4. 反向查询与截断身份

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

### 5. 引擎后端

```toml
[dependencies]
okm = { version = "0.1", features = ["fjall"] }    # 或 "slatedb"
```

- **fjall**（同步）：`FjallStore::open(path)` — 本地 LSM 引擎，单 `Database` 句柄，按需 `persist`。
- **slatedb**（异步）：`SlatedbStore::open(path, Arc<dyn ObjectStore>)` — 对象存储后端；构造 store 用 `slatedb::object_store` 的 re-export，版本永远和 slatedb 内部一致。异步遍历走 `AsyncCollection`。
- **MockStore**：内存 `BTreeMap`，memcmp 序——与真实引擎迭代语义一致，测试套件使用。

### 6. Schema 稳定性测试

用硬编码 hex 锁定物理字节——任何布局漂移都让 CI 失败：

```rust
let fk = edge.forward_key();
assert_eq!(&fk[..2], &[0, 8]); // ns=4、FWD——方向位在 BE 字节对的最低位
assert_eq!(&fk[2..6], &7u32.to_be_bytes());
// ... 完整布局断言见 okm/tests/integration.rs
```

## 项目结构

```
okm-derive/        过程宏 crate：KeyEncode、EdgeEncode（零 I/O）
okm/src/key.rs     KeyEncode trait + PrefixKey
okm/src/edge.rs    KvEdge trait + 方向位头部
okm/src/engine.rs  KvEngine trait + MockStore
okm/src/collection.rs  Collection<S, E> 组装点
okm/src/fjall_backend.rs    fjall 适配（feature "fjall"）
okm/src/slatedb_backend.rs  slatedb 适配（feature "slatedb"）
okm/tests/         integration（MockStore）、fjall_eval、slatedb_eval
docs/adr/          架构决策记录
```

## 设计要点

- **命名空间留在代码**——命名空间字典是编译期常量，永不落 KV。访问模式本身就在代码里（二进制 key、无分隔符、每段宽度由代码定义），ns 在代码里和整套 key 布局在代码里是同一件事。宏在编译期运行，那时没有 KV 可读——字典落 KV 是自举死锁。编号手动分配、append-only、永不复用；见 [ADR-0002](docs/adr/0002-namespace-dictionary.md)。
- **两套布局 regime**——主键定宽（零解析、热路径）；二级索引变长（判别文本放最前，UTF-8 字节序 = 字典序扫描；主键 ID 挟带在 key 末尾，value 留空）。定宽是**结构**的属性不是**数据**的属性；判据是访问模式：纯点查可 hash 成定宽，需要前缀/范围扫描必须保留原始文本。完整论证见 [KV 存储引擎](https://github.com/orbsh/wiki/blob/main/kv-storage-engine.md)。
- **宏层刻意无存储**——encode/decode 是纯 `Vec<u8>` 进出函数；引擎选择与生命周期归装配处（`Collection::new(store)`）。这是每个 derive 保持单 item 纯函数的前提。
- **可移植性**：范式作用在字节层、与宿主语言无关——Python dataclass 写同样的 `encode()` 可复现布局，代价是保障从编译期降为运行时断言。
