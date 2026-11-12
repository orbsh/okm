# 查询配方：okm-query 组合子与 SQL 的对应

本文是 okm-query 的使用配方集：组合子（merge / intersect / difference / union、group_by，加图的 walk）怎么组合成 SQL 世界的熟悉形状，以及两条边界记录——where 的物理/内存分界（前缀、区间、filter）、三种分组的落地差异。机制细节见 internals（[索引机制](internals/index-mechanism.zh-CN.md)）；建模纪律见[建模指南](MODELING.zh-CN.md)。

## 核心立场

okm-core 的义务到「每个访问方法给一条有序流」为止；组合（join、分组、图遍历）是算法层工作，全部在 okm-query，零核心改动——每个输入都是现成的 `scan` / `scan_index` 输出。没有查询引擎、没有计划器：配方是 Rust 代码，声明即执行。

```text
SQL 概念        okm 落地
────────────────────────────────────────────
WHERE (物理)    索引声明 + 前缀/区间扫描（免费，见边界记录）
WHERE (内存)    .filter()（stdlib，无需包装）
GROUP BY        group_by（索引排序承担分组）或 reduce（编译期）
JOIN            intersect（有序键流）+ 每个存活键一次回表
多级 rollup     前缀扫描逐级收窄 group 段
图遍历          walk（一跳 = 一次前缀扫描）
```

## GROUP BY 配方

### 索引排序分组（读时）

把 group 字段放在 `fields(...)` 首位，排序即分组：

```rust
#[ok_index(by_org { fields(org_id, created_at) })]
struct User { org_id: u32, created_at: u64, ... }
```

```rust
// org_id 分组计数：一次前缀扫全量 entries，group_by 单趟折叠
let rows = t.scan::<User_ByOrg>(&[]);
let counts = okm_query::group_by(
    rows.iter().filter_map(|(_, r)| r.as_ref()),
    |u| u.org_id,
    || 0u64,
    |acc, _| acc + 1,
)
.collect::<Vec<_>>();   // 折叠是惰性的；collect 是急切镜像
```

输入有序（每个 scan 都有序），group_by 是单趟的——分组成本 = 索引排序成本，后者在写入时已付。

### 多级 rollup（同一条流的逐级前缀）

group 段是编码进 entry key 的字节，逐级收窄前缀 = 逐级上卷，不需要重扫：

```text
fields(org_id, dept_id, created_at) 的索引：

scan(&[org])                    → 这个 org 的全部（rollup level 1）
scan(&[org, dept])              → org × dept（level 2）
scan(&[org, dept, day])         → org × dept × day（明细）
```

每级都是一次 O(命中) 的前缀扫描；上卷层级按需选，物理布局一次声明覆盖所有层级。

### 三种分组的落地差异（同一批编码器）

| 落地 | 时机 | 语义 | 适合 |
|---|---|---|---|
| `#[ok_reduce]` | 编译期 | 可逆聚合，随写路径维护，常驻 | 高频读的固定分组 |
| `group_by`（索引排序） | 读时 | 单趟折叠，随查询变化 | 即席分组、多维度 |
| reduce 的 GROUP vs 索引首位 | — | — | 同一批字段编码器，不同落地时机 |

判据：分组维度固定且读密集 → reduce（写时付税，读免费）；分组维度多变 → 索引排序 + group_by（读时付税，布局一次覆盖多维度）。两者不互斥：同一索引既服务 group_by 又提供时间线扫描。

## JOIN 配方：求交键，再回表

join 是两步，只有第一步是查询算子。

**第一步——对条目求交。** 两侧都在 join 列上声明了索引，于是两条流本来就按它有序
（索引扫描的序是条目元组 `(字段…, 主键)`，这个元组的任何前缀投影仍然有序）。
只向扫描要求交需要的那几部分——类型参数就是载荷菜单，不写 `Row` 就一次 payload 都不读：

```rust
// 按 user 有序，值内再按主键；(字段段, 主键) —— 不回表
let left = orders
    .scan::<Order_ByUser>(&[])
    .parts::<(parts::Fields, parts::Key)>()
    .map(|hit| hit.map(|(f, _)| f.user_id));
let right = users
    .scan::<User_ById>(&[])
    .parts::<(parts::Fields, parts::Key)>()
    .map(|hit| hit.map(|(f, _)| f.id));

let common: Vec<u32> = left.intersect(right).collect::<Result<Vec<_>>>()?;
```

扫描逐条给条目（每条一个 `Result`），所以在你要求之前什么都不物化：`collect::<Result<Vec<_>>>()` 是
急切那面镜子，`.take(n)` 是 LIMIT，`.rev()` 倒着走。词只有三个——前缀、等值、区间——载荷是类型参数，
所以不会"每种载荷一个词"。

`intersect` 是任何"能吐出键"的东西（`Vec`、数组、扫描迭代器）上的方法，也是两条有序流上的懒归并：
集合语义（两侧重复的键只答一次）、零分配、`take(n)` 立即停。多于两侧就链下去——
`a.intersect(b).intersect(c)`；混合问题同样能链："a 与 b 共有、而 c 没有的那些键" =
`a.intersect(b).difference(c)`。

通用形态是 `merge`：每个不同的键交一个元组——`(Some, None)` 只有左、`(Some, Some)` 两边都有、
`(None, Some)` 只有右：

```rust
for (left, right) in left_keys.merge(right_keys) {
    match (left, right) {
        (Some(a), Some(b)) => both.push((a, b)),
        (Some(a), None) => left_only.push(a),
        (None, Some(b)) => right_only.push(b),
        (None, None) => unreachable!("merge never answers two absences"),
    }
}
```

`intersect` 与 `difference` 就是这条流上的两个 filter，单独命名是因为它们被问得多得多。`union`
是第三个——`a.union(b)` 是 `merge` 把每个元组折叠成它唯一在场的那一侧，于是把两条流交织成一条
有序流，而不是拼接（`chain` 把整个左侧排在第一个右侧键之前，去重的话要么全量物化、要么用
hash set）。

**第二步——回表取幸存者。** 每个公共键去读携带它的行；重复键的笛卡尔积只在这里发生：

```rust
for key in common {
    for order in orders.scan::<Order_ByUser>(&key.to_be_bytes()) {
        for user in users.scan::<User_ById>(&key.to_be_bytes()) {
            emit(order?, user?);   // 这个键的 m 行 × n 行
        }
    }
}
```

每个公共键一次索引探测。键集合在读到第一行之前就已经完整，所以可以先过滤、截断、计数——
选择性在碰任何 payload 之前就生效。

**另一种形状：键流一趟走完（几乎全集相交时）。** 如果两侧几乎完全相交，代价变成逐键探测，那么
两条流一趟配对更划算，哪怕它读了两侧的行。join 仍然分两步：`a.merge(b)` 对每个不同的键交一个
元组——相遇的键就是 `(Some, Some)` 那一种——调用方在自己的循环里把这个元组展开成对，回表也发生
在那里：

```rust
for (left, right) in left_keys.merge(right_keys) {
    match (left, right) {
        (Some(a), Some(b)) => {
            for order in orders.scan::<Order_ByUser>(&a.to_be_bytes()) {
                for user in users.scan::<User_ById>(&b.to_be_bytes()) {
                    emit(order?, user?);   // a 的 m 行 × b 的 n 行
                }
            }
        }
        _ => {}   // 单侧的键就是上面外连接的答案
    }
}
```

没有算子替你展开对，这正是要点：多重性属于这个循环，所以不存在第二套 join 语义要跟着改，没有
两条各长 k 的 run 的 k² 次拷贝，也没有 `.take(n)` 截不断的 `Vec`。和这里其他算子一样，两条流必须
按 join 键有序；键布局不保证有序时先显式排序——一趟走完省掉的是探测次数，不是排序。

**外连接就是差集。** "左边有、右边没有" = `a.difference(b)`——接收者就是左侧，
所以 `a − (a ∩ b)` 不必先算交集——三个答案恰好划分两条流：

```rust
let left_only:  Vec<_> = left.clone().difference(right.clone()).collect();
let right_only: Vec<_> = right.difference(left).collect();
```

N 侧就先求一次交集，再逐侧减：`a.difference(common)`、`b.difference(common)`、
`c.difference(common)`——各得该侧"比交集多出来的那些"。

**一侧远小于另一侧时——而两侧都能跳。** `intersect` 只能推进流，落后的那侧要把空隙里的每个键都走
一遍。扫描可以被“移动”而不是被“步进”：`Collection::seek_scan` 返回实现 `Seekable` 的游标（其
`seek` 就是 `ScanIter::seek`），`okm_query::intersect_seek` 比较两侧位置，短空隙用走的、长空隙用
跳的——先走 `SEEK_BUDGET` = 8 步再 seek，因为一次 seek 是在扫描已持有的 snapshot/事务上重开区间
（微秒级），而一步是纳秒级。它在倾斜输入上划算（一次宽扫描对一个小键集，或对一次窄扫描），此时
归并的逐条推进就是全部成本；两侧齐头并进时它**不**划算——那里 `intersect` 每个键一步，一次重开
都没有。两侧必须走同一个键空间:seek 的边界是原始条目键，所以要么配同一访问方法的两次扫描，要么
配一次扫描与一份用索引自身编码器拼出的键集。

**有序契约的代价。** `intersect` 与 `difference` 要求输入按键有序，并且**不检查**：
检查意味着先把一条流读到尽头再回答，那正是它们要避免的物化。无序输入不会大声出错——
它会静默少键。join 列是索引首位时这个契约是免费的（扫描元组的任何前缀投影都有序）；
不是首位就先显式排序，让那次排序出现在代码里，而不是藏在算子里。

## 图配方：walk

一跳 = 一次前缀扫描（边层保证双向条目存在）：

```rust
// 好友的好友：两跳，每跳每方向一次前缀扫
let found = okm_query::walk(&[&friend_edge], &store, &start_bytes, 2);
```

成本模型与 edge 层的 forward/reverse 完全一致，只多了 frontier 与 visited set。

## 边界记录：where 的形态

- **prefix = 物理 where（等值）**。前缀扫描命中的字节范围就是存储层的过滤，免费（不读不命中）。选择性属于键布局设计——「这个实体按什么查」决定 `fields` 的首位放什么。该进 key 的过滤条件放 `.filter()` 里做 = 放弃存储层的选择性，每次查询为丢弃的行付解码成本。
- **主键前缀 = 同一种 where，不需要访问方法**。合成主键的前导段天生按段分组，于是 `scan_keys` 收一个前缀探针，把「这个父节点的全部子项」答成对主表的一次前缀扫描——`scan_keys(&DeviceKey::prefix().org(7).finish())`，探针按声明序命名字段。全表扫描就是**空探针**（`PrefixProbe::empty()`），不是另一个门。它是惰性的：`.take(n)` 是 LIMIT，只为真正执行的读取付费；`.rev()` 对窗口反向走；答案是**行**的 key（要文档就是一次点读再加调用方的 `to_value` + `merge`；`scan_document_keys` 是急切的两槽模式）。分组本来就在键里时才用它；维度不是前导段，那是访问方法的问题，不是探针的问题。
- **range = 物理 where 的区间形态（ADR-0020）**。谓词落在索引首字段、且字节序 == 值序时，它就是一个 key 区间——`1 < a < 100` 只读 `[1, 100)` 内的行，走 `scan_range`（惰性：LIMIT / 首个命中即停止拉取；`.rev()` 反向读，免费）。边界是首字段的编码字节，由调用方负责。与 prefix 的差异不在成本——两者都只读命中——而在形状：prefix = 首段等值，range = 首字段比较。都不需要 `.filter()`。
- **exact = 全字段等值（`scan_exact`）**。探针必须给出数据段的**全部字段**：吃尽字段段才是命中，于是"值更长"（prefix 的命中）与"探针啃进主键尾段"两侧同时排除——精确匹配不靠回表核验。只给前导字段是分组语义，属 prefix。
- **`.filter()` = 内存 where**。_stdlib 即可，无需包装_：对 scan 返回的输出直接 `.filter()`。谓词无法进键（非前缀维度、跨字段条件、计算谓词）时才落在这里。

判据一句话：**先问这个谓词能否成为某个访问方法的前缀（等值）或首字段区间（比较）；能 → 改索引声明 / 边界，不能 → filter**。前缀和区间都是免费的，filter 是按行付费的。
