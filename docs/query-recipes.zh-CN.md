# 查询配方：okm-query 组合子与 SQL 的对应

本文是 okm-query 的使用配方集：三个组合子（merge_join / group_by / walk）怎么组合成 SQL 世界的熟悉形状，以及两条边界记录——where 的物理/内存分界、三种分组的落地差异。机制细节见 internals（[索引机制](internals/index-mechanism.zh-CN.md)）；建模纪律见[建模指南](MODELING.zh-CN.md)。

## 核心立场

okm-core 的义务到「每个访问方法给一条有序流」为止；组合（join、分组、图遍历）是算法层工作，全部在 okm-query，零核心改动——每个输入都是现成的 `scan` / `scan_index` 输出。没有查询引擎、没有计划器：配方是 Rust 代码，声明即执行。

```text
SQL 概念        okm 落地
────────────────────────────────────────────
WHERE (物理)    索引声明 + 前缀扫描（免费，见边界记录）
WHERE (内存)    .filter()（stdlib，无包装）
GROUP BY        group_by 组合子（索引排序承担分组）或 reduce（编译期）
JOIN            merge_join（双方都是键序流）
多级 rollup     前缀扫描逐级收窄 group 段
图遍历          walk（一跳 = 一次前缀扫描）
```

## GROUP BY 配方

### 索引排序分组（读时）

把 group 字段放在 `fields(...)` 首位，排序即分组：

```rust
#[kv_index(by_org { fields(org_id, created_at) })]
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
);
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
| `#[kv_reduce]` | 编译期 | 可逆聚合，随写路径维护，常驻 | 高频读的固定分组 |
| `group_by`（索引排序） | 读时 | 单趟折叠，随查询变化 | 即席分组、多维度 |
| reduce 的 GROUP vs 索引首位 | — | — | 同一批字段编码器，不同落地时机 |

判据：分组维度固定且读密集 → reduce（写时付税，读免费）；分组维度多变 → 索引排序 + group_by（读时付税，布局一次覆盖多维度）。两者不互斥：同一索引既服务 group_by 又提供时间线扫描。

## JOIN 配方：merge_join

```rust
// 两张表各自按 join key 声明索引（键序流），merge_join 单趟合并
let left = orders.scan::<Order_ByUser>(&[]);    // 键序
let right = users.scan::<User_ById>(&[]);       // 键序
let pairs = okm_query::merge_join(
    left.into_iter().map(|(k, _)| (user_bytes(&k), ())),
    right.into_iter().map(|(k, _)| (k, ())),
);
```

刻意没有 hash join：join key 不是某一侧的排序维度 = 建模缺口——声明 `kv_index(fields(join_key))`，流重新有序。排序是存储的属性，不是查询算子的负担（merge join 流式、内存有界；hash join 必须物化 build 侧）。

## 图配方：walk

一跳 = 一次前缀扫描（边层保证双向条目存在）：

```rust
// 好友的好友：两跳，每跳每方向一次前缀扫
let found = okm_query::walk(&[&friend_edge], &store, &start_bytes, 2);
```

成本模型与 edge 层的 forward/reverse 完全一致，只多了 frontier 与 visited set。

## 边界记录：where 的两种形态

- **prefix = 物理 where**。前缀扫描命中的字节范围就是存储层的过滤，免费（不读不命中）。选择性属于键布局设计——「这个实体按什么查」决定 `fields` 的首位放什么。该进 key 的过滤条件放 `.filter()` 里做 = 放弃存储层的选择性，每次查询为丢弃的行付解码成本。
- **`.filter()` = 内存 where**。_stdlib 即可，无需包装_：对 scan 返回的 `Vec<(PrefixKey, Option<R>)>` 直接 `.filter()`。谓词无法进键（非前缀维度、跨字段条件、计算谓词）时才落在这里。

判据一句话：**先问这个谓词能否成为某个访问方法的前缀；能 → 改索引声明，不能 → filter**。前缀是免费的，filter 是按行付费的。
