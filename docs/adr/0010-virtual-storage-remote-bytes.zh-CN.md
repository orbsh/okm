# ADR-0010: VirtualStorage —— KvEngine 即存储边界，远端字节走既有通道

Date: 2026-09-13
Status: Accepted

## Context

三个独立需求汇聚到同一机制：

1. **Krystallizer 托管在 Aura 上** —— Krystallizer 的存储要能落在 Aura 节点（远程部署），且不改存储语义。它的引擎选型至今：mock / fjall / slatedb。
2. **多语言 Actor 需要 KV 语义** —— Python/Steel Actor（嵌入式、零 IPC 拓扑）要 OKM 的编码纪律但不能手拼 key。它们需要一个 schema 驱动的动态 codec。
3. **多租户隔离** —— 多个应用共享一个 Aura 节点的物理引擎；隔离必须是结构性的，不是命名过滤。

第一直觉是做「远程 KV 协议」：声明端点 + 专用 TCP 服务。它在三条上全失败：通道已存在的情况下分叉第二套传输（Probe→控制面是出站 WS 连接；域内调用是 realm 事件），且在数据已经是编码字节的地方制造了一个协议。

## Decision

### 1. trait 就是边界 —— VirtualStorage

`KvEngine`（put/get/del/scan_suffix/batch/commit_batch）只说编码后的话。它自此就是存储边界点，别名/概念化为 **VirtualStorage**。一个 trait 后面四种后端形态：

- `mock` —— BTreeMap（测试）
- `fjall` —— 本地引擎（feature）
- `slatedb` —— S3 引擎（feature）
- **remote** —— op 走既有通道；字节进字节出

加后端 = 多一个 `impl KvEngine`。trait 不加方法。

### 2. 远端发送已编码的字节

OKM 的写路径本来就是双写（一次 batch 内主表 + 索引条目）。线帧就是那个 batch：`commit_batch` 的 `MemBatch` op 列表序列化（postcard）—— `[op][batch bytes]`。全程无语义解析：

- **发送方**（如 Krystallizer）：key/value 在 trait 边界处已编码；帧只包住 op 边界。它对接收方的前缀一无所知。
- **接收方**：前置自己声明的前缀，按普通字节级 KV 引擎执行（一个 batch 一次真实 WAL commit），扫描结果按原始字节回填。它不解析 key，不认识 TLV 帧，不知道 OKM 存在。存垃圾与存数据不可区分——by design。
- **读路径**（`get`/`scan_suffix`）需要响应——请求与应答如何关联是消费方的事，不是线协议的事。发送方实现 trait（`get` 返回 `Option<Vec<u8>>`）；在这个签名下用什么传输、什么关联 id、回调还是阻塞等待，都是它自己的选择。线上承载请求帧和响应帧；以上皆在本 ADR 之外。
- **线上没有版本头。** 布局版本化是发送方的进程内事务（编译期 hex 测试 + `#[kv_layout(version)]` 解码拒绝）。Aura 不运行发送方的 OKM，不持有布局知识；两端唯一的契约是 op 字节 + 字节流。（此条推翻了早期草案帧头放布局版本的设计——错在把两个刻意无关的端语义耦合了。）
- **原子性**：一帧 = 一 batch = 接收方一次 WAL commit。跨 batch 顺序 = 通道顺序。无 epoch，无协商。

### 3. OKM 实例互不相关；接收方桥是唯一交叉点

Aura 自己的应用数据、Krystallizer 的记忆图、未来的 Python/Steel Actor（dynamic OKM）各自运行自己的进程内 OKM、自己的声明 schema。这些实例共享 nothing。唯一交叉点是 Aura 的远程存储接收方——它向远程 VirtualStorage 后端服务字节流，且不理解任何内容。

Dynamic OKM 不过这个桥：同进程 Actor 直接持有引擎；动态 codec 是从 `describe()`/`json_schema()` 导出生成的 schema 驱动编解码器，进程内使用。能力天花板，永久的：dynamic 侧无 reduce/subscribe（fold/unfold 是 Rust 编译期逻辑，动态重建会破坏恰好一次）。

引擎选择是组装点级的，且可自由混用：本地 Table 绑本地引擎，同进程另一个 Table 绑 remote 后端（「转发」——语义相同，只是引擎选了 remote）。没有需要防守的混用冲突：接收方前缀是接收方套的封装，不是发送方键形的组成部分，所以本地 `[ns 2B]` 键与托管 `[prefix][ns 2B]` 字节不会在同一引擎里相遇——除非接收方自己选择把它们放一起。无保留前缀值。

### 4. 接收方声明：`#[kv_storage]` derive

接收方是声明出来的，不是手接的。空结构体标注 `#[kv_storage(...)]` 只声明前缀（服务哪个应用/前缀）。derive 生成**无数据方法**（无 put/get/scan——没有可编码的行类型），只有一个执行方法（`exec`/`receive`）：收帧 → 前置声明前缀 → 普通引擎执行 → 回填扫描结果。

一次钉死三件事：前缀来源（声明，不是散落配置）、执行器形状（宏生成，不是手拼）、前缀逃逸的结构不可能（句柄构造时绑定前缀；跨命名空间不可表达——物理分隔，不是命名过滤）。与 `#[kv_subscribe]` 同一纪律：注解声明事实，宏生成实现。

### 5. 多租户：接收方前缀，一个实例一个 ns

多租户的数据库级机制只有一种：接收方声明的前缀。一个远程 OKM 实例 = 一个应用 = 一个领域模型 = 自己的 ns 字典；对接收方它只是又一个前缀。OKM 内部没有 `app_id` 层，无保留前缀值，无多级 ns 声明——应用内部要按租户分片，`tenant_id` 是它结构体里的普通 key 字段（业务分片，所有租户同一建模）；只有当整个应用在平台层隔离时，接收方才为每个应用托管一个 `#[kv_storage]` 执行器，各自声明前缀，各自后面挂自己的单级 ns 字典。

接收方引擎上的物理键是纯拼接——接收方字节在前，发送方字节在后，顺序永不调整：

```text
[receiver prefix][ ns 2B ][ sender's key payload ... ]
 └─ 接收方 ──┘  └────── 发送方字节，原样不动 ─────┘
```

接收方只知道自己的前缀；ns 段是发送方的——接收方前置前缀后从不去解析后面是什么；ns 字节不透明地穿过。知识不对称即隔离机制：接收方无法越过看不见的东西路由，发送方无法逃出自己不持有的前缀（`#[kv_storage]` 句柄构造时绑定前缀）。ADR-0002 的「若租户存在」条款由此**从草图提升为正式机制**；它当初否决租户外侧隔离的前提是「无用户可编程查询面」，dynamic 执行模式改变了这个前提——本 ADR 即修订触发器。ns 字典纪律本身（编译期、append-only、u16）不动。

### 6. 传输是后端内部细节

同进程：直接引擎句柄（没有 remote 后端）。
同机跨进程：进程内通道 / UDS。
跨机器：既有出站 WS 连接（Probe 情形）或 realm 事件（域内情形）。发送方持有「一个实现了 trait 的对象」；物理拓扑组装期定死。无声明端点，无专用监听器，无第二协议。

对称地，接收方也不知道到达路径。`#[kv_storage]` 执行器的面恰好一个方法——帧进，结果出；谁调它、走什么通道是调用方的事。读关联（哪个响应回答哪个请求）在执行器的发送侧，同写侧。TCP + postcard 客户端是参考示例，不是契约的一部分。传输多样性只存在于执行器两侧的外部，永不漏进执行器。

## Consequences

- Krystallizer 的存储配置变四选一：mock / fjall / slatedb / virtual(→Aura)。OKM 语义层（Table/index/reduce/事件）不变。
- Aura 增加一个 Storage Actor 托管 `#[kv_storage]` 执行器：每应用一个声明实例（各一个声明前缀），帧来自 VirtualStorage 后端或 realm 事件；同机调用方进程内连接。
- 帧格式最小且稳定（`op + bytes`）；它不是 OKM 实例间的兼容面——只是发送方 trait 边界与接收方引擎之间的。OKM 编码演化无需改帧。
- 写侧串行是接收方引擎的普通单写者行为，不因远程而改变。
