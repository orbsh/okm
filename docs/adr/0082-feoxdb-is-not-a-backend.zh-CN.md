> **Languages:** [English](0082-feoxdb-is-not-a-backend.md) (primary) · [中文](0082-feoxdb-is-not-a-backend.zh-CN.md)

# ADR-0082 — FeOxDB 不作为后端，持久与临时都不作

**Status：** 2026-11-24 裁决（无代码）。关闭 FeOxDB 0.6.0 作为两个角色的候选。

## 1. 提案是什么

FeOxDB（`mehrantsi/feoxdb`，Apache-2.0，crates.io `feoxdb` 0.6.0，2026-07-31 发布）是一个纯 Rust 嵌入式 KV，宣传纳秒级读，两种形态：纯内存，以及带写后台缓冲的持久。内部是一个 `scc::HashMap`（默认 23 位哈希、8M 桶）、一个 `crossbeam_skiplist::SkipMap` 作有序索引，每条目一个 `Arc` 记录（携带值、TTL、版本时间戳）。写面全是单键：`insert`、`delete`、`insert_if_absent`、`compare_and_swap`、`atomic_increment`、TTL 与 JSON patch 变体，加一个 `flush`。

提案是把它接成可选引擎；又因为它也跑纯内存，顺带当作 ADR-0072 给 `EphemeralStore` 那个非持久模式的候选。两个角色都否，而且都不是嫌它慢：它比 redb 还少链 484 KB。

## 2. 量到了什么

链接成本，`rustc 1.100.0`，`lto = "thin"`，`codegen-units = 1`，strip，release，各自一个二进制：

| 二进制 | strip 后 | 相对裸基线 | 依赖闭包（去重 crate 数） |
|---|---|---|---|
| `bare` | 320,680 B | — | — |
| + `feoxdb` 0.6.0（no default features） | 766,080 B | +445 KB | 52 |
| + `redb` 4.2 | 1,250,144 B | +929 KB | 1 |

真正花钱的是依赖闭包：52 个 crate，含 `io-uring`、`json-patch`、`serde_json`、`intrusive-collections`、`scc`、`ahash`、`crossbeam-*`、`nix`/`libc`、`rand`。ADR-0064 否掉 SurrealKV 的一条是「多 150 个 crate 换 okm 没有消费者的能力」；这里 52 个买来 TTL、CAS、原子计数、RFC 6902 补丁，okm 一个都不调。

纯内存模式 `FeoxStore::new(None)`，4 万条目，release，系统分配器（不是它默认的 jemalloc），RSS 取自 `/proc/self/statm`：

| 语料 | RSS/条 | 自报 `memory_usage()`/条 | 插入 4 万条 |
|---|---|---|---|
| 索引面——19 B 键 / 1 B 值 | 3,916 B | 188 B | 47–65 ms |
| 行——11 B 键 / 120 B 值 | 4,007 B | 299 B | 50–64 ms |
| 行——11 B 键 / 2 KB 值 | 5,954 B | 2,227 B | 67 ms |

固定开销由第三行露出来：值多 1,928 B，RSS 只多 1,947 B，所以一条记录在载荷之外要 ~3.9 KB 进程内存——每条一块的记录，而它自己的账只数记录体，既不数索引也不数块。1000 键点读 470–564 µs，1000 键 `range_query` 96–128 µs，`new()` 24–43 µs（RSS +350 KB）；20 轮「建 → 插 1000 → drop」后 RSS 停在 19.7 MB 的地板，不随轮次增长。

对上 ADR-0072 用计数分配器量出的同一批族——它那边索引面是 16 B/空、行是 11 B/120 B——`EphemeralStore` 是索引面 25 B/条、行 137 B/条，比 FeOxDB 的内存模式**密度高 157 倍与 29 倍**（探针的索引键是 19 B、值 1 B，因为引擎不收空值）；点写 86–211 ns 对它 1.2–1.6 µs（10 倍），3 万条构建 3.5 ms 对它每条约慢 10 倍。

## 3. 三条不是适配工作能解决的约束

1. **不收空值。** `operations.rs:264` 与 `:602`（v0.6.0）：`if value.is_empty() || value.len() > MAX_VALUE_SIZE { return Err(InvalidValueSize) }`。写 `b""` 的探针当场报 `InvalidValueSize`。没有 `includes` 的索引条目存的正是空值，ADR-0072 自己的表把这一族写成「16 B / empty」——那是一个 store 里数量最多的一类条目。塞一个占位字节、读时再剥，是让编码去迁就引擎，方向正与 ADR-0011、ADR-0018 相反。
2. **`Ok` 不等于字节在盘上。** ADR-0039 §1：`put` 答 `Ok(())` 的意思是 store 手里有这些字节，不是「调用被接受了」。FeOxDB 是写后台缓冲——每 100 ms 或每分片 1024 条/16 MB 刷一次，最坏 `16 MB × 分片数` 在险（4 分片 64 MB、8 分片 128 MB）——它自己也这么写（`lib.rs:47-49`）。要满足契约就得每次写后 `flush()`，那就把它存在的理由删掉；不满足，则 ADR-0045 的「先提交后追加、至多一次」变成幽灵行：日志里有一条 store 已经忘了的行。
3. **没有多键原子提交。** 公开面没有 batch、没有事务、没有 commit，每次写一个键。okm 的写路径是主行 + N 条索引面 + reduce 折叠在**一个** `commit_batch` 里（ADR-0003、ADR-0031），而 trait 默认实现只是逐条重放，因为它假定每个真引擎都会用原生原子单元覆盖（fjall `Batch`、redb 一个写事务）。在 FeOxDB 上默认实现就是全部，崩一次落半个文档。

## 4. 就算这三条修好，接口还得改

`range_query(start, end, limit) -> Result<Vec<(Vec<u8>, Vec<u8>)>>`（`range.rs:45`）**两端都是闭区间**（`:16`）且把结果物化。okm 的 `scan_range(begin, Some(end))` 是**排他**上界——通常就是前缀的下一个字节——不给闭区间的 API 就只能多扫一行再丢掉尾巴。迭代只有正向（`lower_bound` + `next()`），而 okm 的契约测试每个 arm 都要反向走一遍（`scan_range_test.rs` 的 `.rev()`）。这两条可以靠物化成 `ScanIter::Buffered` 绕过去，而那正是 ADR-0072 拒绝的缓冲扫描：一个大区间上的 `LIMIT 10` 要为整个区间付账。

另外 `insert` 用版本时间戳做冲突消解，带旧戳的写会被拒（`OlderTimestamp`）——okm 的 `put` 是覆盖语义，不该因为墙上时钟回拨而失败。

## 5. 为什么临时引擎也不行

ADR-0072 已经用测量裁决过这个机制：第三方有序容器都是对 `K`/`V` 泛型的，变长键在它那里就是一个 owned `Vec<u8>`（索引面 109 B/条），而它的区间迭代器借用容器，进不了 `'static` 的 `ScanIter`——唯一的入口是物化，也就是这个 store 存在的意义所要避免的那次内存尖峰。跳表那边落地过又被换掉：同一族 48 B/条，打包 B+tree 31 B/条。

FeOxDB 就是这个形状，外加每条记录一块页：3,916 B/条，而被否的跳表在同族量到 48 B。内存模式还继承了空值拒绝（同一个 `insert` 路径），所以两种角色里索引面都得塞占位字节。再加上默认 4 GB 上限，大约一百万条就 `OutOfMemory`——而 okm 的测试矩阵每个用例建一个 store。

## 6. 裁决

FeOxDB 不是 okm 的后端，两种模式都不是。不排期、不温着当候选、也不进引擎清单；`VirtualStorage` 一字不改。它确实有的能力——亚微秒读、TTL、CAS、JSON 补丁、内存缓存——是缓存层的能力，宿主想要就把它放在 okm 旁边，而不是垫在底下。

**重开条件：**四条能力全到位，外加一个消费者。引擎需要多键原子提交、`Ok` 即落盘的持久模式、接受空值，以及一个能排他上界、又能反向走的惰性区间；同时得有人真的需要一台 okm 尚未携带的引擎。基准重不开任何一条：这次拒绝是语义的，而它在每一列数字上都已经是差的那一侧。

## 7. 被否的替代

- **给空值塞占位字节。** 让 okm 的编码向一个引擎的拒绝低头，并且把这个差别永久留在读路径上（ADR-0011 把构造保持单点）。
- **每次写后 `flush()`。** 把 1.2–1.6 µs 的插入变成 fsync 绑定，而且 §3.3 仍在：每次一个 flush 不等于每个文档一次提交。
- **每次 `commit_batch` 刷一次。** 更接近了，但刷到一半崩掉会留下这批的一个前缀；原子性是引擎的承诺，否则谁的都不是。
- **只用作非持久引擎。** 为那个模式而存在的引擎是 157 倍与 29 倍的密度差、写入 10 倍时延，而且空值仍然被拒。
- **作为「已记录、未设计」继续挂着。** 它已经量过；挂着暗示答案未定，而答案是三条契约违背加两个数量级的差距。