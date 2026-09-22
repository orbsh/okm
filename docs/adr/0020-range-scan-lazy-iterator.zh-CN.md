# ADR-0020: 范围扫描是引擎唯一的有序读原语——trait 边界上的惰性迭代

日期：2026-09-21
状态：已接受（实现随本决策的提交落地；Collection 层语义与文档随同批次）。

## 背景

引擎契约原本只有一种有序读形态：`scan_suffix(prefix) -> Vec<Vec<u8>>`——等值前缀、一次性物化成向量。三个后果：

1. **范围谓词没有物理落点。** 索引字段上的 `1 < a < 100` 只能全索引（或全表）扫回内存过滤——正是「前缀 = 物理 WHERE」边界记录要防止的退化。字节序 == 值序的知识早已内建于每种编码（定宽大端；VarInt 刻意做成 prefix-monotonic），但引擎 API 无法利用它：区间不是前缀。
2. **没有提前退出。** 即使是前缀扫描，只要前 10 条命中的消费者也得为全部命中买单：Vec 在 trait 边界之内就完全物化了，调用方拿到第一条之前成本已经付清。
3. **引擎早就有这个能力。** fjall 3.1.10、redb 4.2.0、slatedb 0.16 都原生支持键区间惰性迭代（调查见下）。把能力藏起来的只是 trait 的形状。

### 引擎 API 调查（2026-09-21，版本为本仓实际消费版本）

| 引擎 | range API | 边界 | 迭代器 | 所有权 |
|---|---|---|---|---|
| fjall 3.1.10 | `Keyspace::range(R: RangeBounds)` / `prefix(p)` | 完整 key 的 `RangeBounds`，两端都可省 | `Iter: Iterator<Item = Guard>`——惰性、DoubleEnded | **owned + 'static**：`Iter` 内部持有 snapshot nonce；`Guard::into_inner()` 产出 owned `(UserKey, UserValue)` |
| redb 4.2.0 | `Table::range(RangeBounds)` / `range_owned(RangeBounds)` | 完整 key 的 `RangeBounds` | `Range<'a>` 借读事务；`OwnedRange` 是 'static（Arc 化的事务 guard） | **经 `range_owned` 获得 owned + 'static**——guard 在 `ReadTransaction` 句柄 drop 后保住页 |
| slatedb 0.16 | `Db::scan_prefix(prefix, subrange: ByteRangeBounds)` | 前缀 + 相对前缀的子区间（空前缀 = 全键空间区间）；`ByteRangeBounds` 对所有 `Range<T>` 形状（`AsRef<[u8]>`）有实现 | `DbIterator`——异步 `next().await`，惰性 | **owned + 'static**（内部 boxed 迭代器）；同步适配需要 owned 运行时句柄（`Arc<Runtime>`），每次 `next()` 一次 `block_on` |
| wire/远端（ADR-0010） | 单条 `OP_SCAN` 帧 | key 段承载 begin；**value 段原本为空** | 响应一次性缓冲整个答案 | 构造上即缓冲——帧是消息，不是流 |

这份调查提前定了两个本来要争的问题：所有原生迭代器都是 owned 且 `'static`，所以 boxed `dyn Iterator` 的 trait 方法不损失任何东西（没有生命周期把戏、没有句柄泄漏）；远端路径不改 wire 协议就无法流式，必须允许它降级。

## 决策

**一个原语：`scan_range`。一个流式形态：`scan_range_iter`。**

```rust
pub trait VirtualStorage {
    /// 唯一的有序读原语：完整 key 上的 `[begin, end)` 字节序区间；
    /// `None` end = 无上界；begin 含端点。
    fn scan_range(&self, begin: &[u8], end: Option<&[u8]>) -> Vec<Vec<u8>>;

    /// 惰性形态：完整 (key, value) 对按需拉取。
    /// 默认 = 在 `scan_range` 之上的缓冲降级；拥有原生惰性迭代器的
    /// 引擎用真迭代覆盖它。
    fn scan_range_iter(&self, begin: &[u8], end: Option<&[u8]>)
        -> ScanIter;
}
```

- **`scan_suffix(prefix)` 降为 `scan_range` 之上的默认方法**，经新的 `prefix_end(prefix) -> Option<Vec<u8>>` 辅助（末字节进位；前缀全 `0xFF` 时为 `None`）。前缀语义搭在区间原语之上，而不是第二个物理操作。引擎可以保留自己的 `scan_suffix` 覆盖——原生前缀扫描更快时（slatedb 的 `scan_prefix(prefix, subrange)` 就是一例）。
- **惰性形态是独立方法，不是改返回类型。** `scan_range` 保持 `Vec`（现有全部消费者期望的缓冲形态）；`scan_range_iter` 返回 `ScanIter`——半路放弃（LIMIT、首个命中、提前 drop）只为实际执行的读买单。原生迭代器是惰性的引擎覆盖 `scan_range_iter`；trait 默认实现先物化（借用无法跨越 trait 边界）并如实文档化。
- **远端路径刻意降级。** `RemoteStore` 走同一条 `OP_SCAN` 帧：value 段——前缀扫描时为空——携带 `[0x01][end 字节]`（有有限上界）或 `[0x00]`（无上界）。零 wire 变更（帧本来就有 value 字段；ADR-0010 的「wire 只见 op tag」成立）。远端引擎上的 `scan_range_iter` 按帧的本性缓冲响应。流式化远端路径是 wire 协议决策，本 ADR 刻意不做。
- **空区间是合法答案。** slatedb 对空 `Range`（end <= begin）panic；适配器返回空——guard 放在适配器里，不推给调用方。

**为什么不合成一个返回 `Box<dyn Iterator>` 的方法？** 缓冲与惰性两种形态的失败模型和成本模型不同：缓冲扫描是快照大小的分配、成本固定；惰性扫描是调用方可中途放弃的开放游标（fjall 的 snapshot nonce、redb 的事务 guard、slatedb 的迭代器在 drop 前各自持有引擎资源）。两个名字说清调用方要的是哪个契约；合成一个名字会逼每个现有 `scan_suffix` 消费者去关心它从没要求过的资源生命周期。

**`ScanIter`：不透明枚举，不暴露引擎类型。** 惰性形态无法返回 `Box<dyn DoubleEndedIterator>`——该 trait 不是对象安全的。出路是 okm-core 里的一个具体具名类型：

```rust
pub enum ScanIter {                       // 变体只在 okm-core 内部可见
    Fjall(fjall::Iter), Redb(redb::OwnedRange<..>),
    Slatedb(SlatedbIter), Buffered(..),   // + trait 默认 / remote 臂
}
impl Iterator for ScanIter { type Item = (Vec<u8>, Vec<u8>); }
impl DoubleEndedIterator for ScanIter { /* 每臂各自的 next_back */ }
```

下游只见 `next` / `next_back` / `rev`，既匹配不了也不需要知道底下是哪家引擎在迭代。enum 是 Rust 对象安全问题的绕行（不存在 `dyn DoubleEndedIterator`），而这层枚举正是它不泄漏的原因：加引擎臂是 okm-core 内部事件，下游零感知。反向遍历：fjall 与 redb 原生支持倒序（上方 API survey 已核实——fjall `Iter` 与 redb `OwnedRange` 均实现 `DoubleEndedIterator`）。slatedb 的异步 `DbIterator` 只能正向；其 `next_back` 将剩余区间一次性物化后倒序取出——引擎给不出的惰性在此丢失，语义不变。

**为什么不把前缀当主原语、把区间编码成前缀技巧？** 区间的上界一般不是任何东西的前缀（`a < 100` 的终点落在字节空间中间）；硬合成意味着每个调用点做字节级末值算术。引擎原语是区间；前缀是派生的便利形态——与所有后端的原生实现一致。

## Collection 层

`Collection::scan_range<I>(begin, end)` 把编码后的边界拼接在 entry 头与身份尾之间——`entry_prefix` 填等值前缀的同一区域——于是首索引字段上的 `1 < a < 100` 谓词成为物理 key 区间。`Collection::scan_range_iter<I>` 是惰性镜像。两个名字保持分离（与 trait 一致）：`scan` = 等值前缀语义；`scan_range`/`scan_range_iter` = 调用方拥有边界编码。最左前缀组合照旧：`(a, b)` 索引上对 `a` 取区间、`b` 留空前缀，就是现有前缀纪律的宽化形态。

## 后果与边界

- 每种 OKM 编码必须维持字节序 == 值序，`scan_range` 才有意义。定宽大端与 prefix-monotonic VarInt 今天就成立；未来任何违反它的字段类型自动被排除在范围谓词之外（且应在声明期拒绝）。
- 远端路径的 `scan_range_iter` 在 wire 获得流式帧之前是缓冲的——记为 future work，不是义务。
- `VirtualStorageAsync` 获得同样的异步形态对（那里的 `scan_range_iter` 是后续项；异步 junction 面足够小，今天缓冲无害）。
- **`Reverse<T>` 失去降序读的垄断，不是失去合法性。** `Reverse<u64>` 时间戳把降序烙进 KEY——entry 物理上按新到旧排序，惠及每种扫描形态的每个读者；在布局需要表达新旧语义的地方它仍是必需。`scan_range_iter(..).rev()` 在查询期对未包装字段表达同一种读：零 wire 变更，方向按查询选定。两者都保留；选择规则是布局（查询频繁、方向固定、值得在每条 entry 里付费）对比查询（偶发或方向会变的反向读）。`Reverse<T>` 词汇表原样保留——它只是不再是「新到旧」的唯一读法。
- LIMIT 类消费者是动因，但本 ADR 不加查询规划器、不做谓词下推：边界就是字节，调用方编码它们，`Collection::scan_range` 与 `scan` 共享同一条缝的便利封装。
