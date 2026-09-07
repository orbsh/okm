# ADR-0007: DataFrame 桥 — Arrow RecordBatch 作为唯一接触面

日期：2026-09-07
状态：已接受（设计；实现待做）
英文版：[0007-arrow-dataframe-bridge.md](0007-arrow-dataframe-bridge.md)

## 背景

OKM 的查询面是 KV 极端：访问路径在键设计期定死，没有优化器、没有查询语言。分析类负载（多维聚合、对物化行的即席过滤）不适合这个面——它们正是 DataFrame 语义（可组合方法链、进程内惰性计算图）存在的原因。两者是载荷分工而非竞争层：KV 提供持久化 + 行级访问（OLTP），DataFrame 提供内存分析层。缺的是两者之间的接口。

Polars 是 DataFrame 侧选定的消费方（纯 Rust、Arrow 原生、惰性优化）。Apache Arrow 是天然接触面，理由有三：它是 Polars 的原生内存格式，是 Parquet（快照路径）的物理基底，也是 OKM 指针切片式解码在列存世界的最近似物（固定偏移、零解析）。一个格式对齐三者，任何位置都不出现中间转换税。

## 决策

**桥是只读路径，建立在 Arrow `RecordBatch` 之上。** `Table` 获得把行流式产出为 `RecordBatch` 的能力，Polars 直接消费。具体分两期：

### 第一期 — eager 导出：`Table::to_record_batches()` / `to_polars()`

- 经现有 `KvEngine` 扫描面扫描主段（slot 0），解码每条记录（键字段 + TLV payload），按批次产出列（目标约每 `RecordBatch` 8192 行）。
- **Schema 由 `RowEncode` 生成，不做运行时推断**：字段名来自行声明，类型映射固定（`u32`→`UInt32`、`u64`→`UInt64`、`[u8; N]`→`Binary`，wrapper 按各自读路径——`Enum<T>` 以整数宽度解码，`Offset<T>`/`Delta<T>`/`Quant<T>` 以还原后的逻辑值出现，`Reverse<T>` 在产出前反转）。这延续 "code as DDL"：一个 struct 是键编码、payload 编码、索引槽、快照列、Arrow schema 的单一来源——五个目的地，零漂移面。
- 行→列转换是 O(N) 且不可避免的一次布局转换（行存→列存）；eager 档按每次查询付这笔代价，换的是最高新鲜度。
- `polars` 是可选 Cargo feature（默认关闭）；核心只带 `arrow-*` 依赖返回 `RecordBatch`，不需要桥的构建不为此付出任何代价。

### 第二期 — 惰性下推（推迟，负载验证后决定）

自定义 `LazyFrame` source，让 Polars 优化器把 filter/投影下推进 OKM 扫描（谓词命中某个 `#[kv_index]` 左最前缀 → 索引扫描 + 回表；否则全段扫描）。这是可组合性的完整兑现——方法链的分叉/合并在 Polars DAG 里发生，数据源保持嵌入式零 RTT——但它依赖 `polars-plan` 内部接口，且只有当分析谓词真的命中已声明索引维度时才有收益。等 eager 档揭示真实负载形状后再做。

### 第三期 — 规模化仍走快照路径（ADR-0006 已定）

数据超出内存或分析变成重复热路径时，按查询的行→列转换不再划算：用既有快照导出（rows → Parquet），Polars `scan_parquet` 直读。KV 仍是 source of truth，Parquet 是转换一次、容忍陈旧的分析快照。两档互补：进程内桥 = 每查询付转换、买新鲜度；快照 = 一次付转换、买吞吐。

## 边界

- **只读。** 所有写入走 `Table::put`——主键 + 索引条目一个批次的写契约不能从 DataFrame 侧绕过。需要持久化的分析产物是显式的新行类型，走正常写路径落回。这保证 KV 侧是唯一 source of truth，杜绝双写漂移。
- **接受单机天花板，不与之对抗。** 桥面向进程内、中规模数据（组合阶梯中的 KV + DF 档）。超过之后答案是快照/湖仓路径，不是在 OKM 内长出分布式下推——OKM 不长出 Spark。
- **索引不经桥导出**（与快照同理）：派生状态，确定性重建；DataFrame 只看到行。

## 后果

- 新模块 `okm/src/arrow_bridge.rs`（或 `arrow_backend.rs`，对齐引擎后端命名），挂在 `arrow` feature 下；`to_polars()` 在其上的 `polars` feature 下。
- **导出由 feature 门控**：桥及其依赖（`arrow-*`、`polars`）全部是可选 feature，默认关闭——不需要分析路径的构建（嵌入式 OLTP、CI、嵌入式设备）不为此付出编译时间和二进制体积；`Table` 上的导出方法只在 feature 激活时生成，核心 API 面零增量。
- **ns ID 还原为表名**：RecordBatch 的列不含 ns 字节，但批次需要知道来源表——导出方负责把键里的 `[ns 2B]` 头经 ns 字典（ADR-0002，字典活在代码里）反查回描述性表名，作为批次元数据（表名/列归属）。与 ADR-0006 快照的自描述输出是同一条纪律：二进制 ns 头不得泄漏进消费侧，逆映射由导出方独占。
- `RowEncode` 多产出一个编译期产物：Arrow `Schema` + 每字段列 builder，与 `PAYLOAD_FIELDS` 同源派生。宏复杂度有界：一个 item、一份字段表、多一个输出。
- 变长 payload 字段（String，PLAN Phase 2 待做）天然映射 Arrow `Utf8`——桥不被它阻塞，字段落地时映射就位。
- Wiki 文档（query-language-design.md 的 KV + DF 段）加一句：接触面是 Arrow RecordBatch，规模化出口是 Parquet 快照。不展开设计论述——本 ADR 管设计。
