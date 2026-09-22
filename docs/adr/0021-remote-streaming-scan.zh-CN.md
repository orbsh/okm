# ADR-0021: wire 层的流式扫描——远端路径加入惰性契约

日期：2026-09-22
状态：草案（设计提案；帧形态评审通过后落地实现）。

## 背景

ADR-0020 把惰性迭代定为 `VirtualStorage::scan_range_iter` 的流式契约：每个本地引擎包装自己的原生 owned 迭代器，提前放弃的消费者（LIMIT、首个命中）只为实际执行的读买单。只有一个后端被豁免：远端路径。`RemoteStore::scan_range_iter` 是缓冲的——它走同一条 `OP_SCAN` 帧，`OpResponse` 把整个答案装在一条消息里。ADR-0020 将此记为 future work、非义务。现在排期：缓冲不是假设性成本，它正是 ADR-0020 在其他所有地方消灭的那种退化——远端消费者要「一百万条里的前 10 条」，得先为一百万条付完 wire 上的成本，才看得见第一条。

### 约束集（任何方案必须保住的东西）

这些是 wire 契约已经锁定的性质（ADR-0010）；破坏其中任何一条的流式设计按构造即被否决：

1. **帧是通道的不透明载荷，传输层永不解析。** 所有分块、确认、游标机制必须活在 okm-wire 的帧语法之内——WS/UDS/mpsc 适配器保持「一条 `Vec<u8>` 进，一条 `Vec<u8>` 出」。
2. **执行面是唯一一个方法**：`NestStorage::apply(frame) -> Option<OpResponse>`。流式设计不得要求主机在两次 `apply` 之间为某个消费者保存状态——`apply` 按契约无状态（接收方承载引擎，不承载会话；ADR-0010 §7）。
3. **读关联活在传输信封的发送侧**，绝不进帧字节（ADR-0010 §6）。mpsc 参考传输在信封里带 `(reply_tx, frame)`；WS 适配器靠信封 reply-to 关联。帧格式本身没有 request id。
4. **wire 零 OKM 语义**：key/value 是不透明字节；codec 只认识 op tag 和长度分桶，别的什么都不知道。
5. **写批与写序已经解决**：同一帧内的变更 op 一起提交；scan 排在其后执行（读可见同帧先行的写）。流式重构不得重排或拆散这条保证。

### 今天到底要改什么

`OpResponse` 只有两个字段——`value: Option<Vec<u8>>`（get）与 `suffixes: Vec<Vec<u8>>`（scan），都是整答案形状。scan 答案还丢掉了 value：wire 上的 `scan_range` 只返回 key 后缀，发送端的 `scan_range_iter` 要对每条 key 补一次 `OP_GET` 往返（`nest.rs` 的缓冲路径就是 `scan_range` + N 次 `get`）。所以远端惰性路径今天不只是没流式化，而是在缓冲之上还有 N+1 往返。

## 决策

**一个新 op tag——`OP_SCAN_STREAM`（tag 4，保留 tag 空间的首次启用）——加上复用 `OpResponse` 编码的分块应答语法，而不是第二种应答类型。**

### 请求：`OP_SCAN_STREAM`（一 op 一帧）

与 `OP_SCAN` 同一 op 形状：key 段 = 全 key 的 begin 边界（接收方侧施加托管前缀，与今天一致）；value 段 = ADR-0020 现有的 `[flag][end]` 编码（`0x00` 无上界、`0x01` + end 字节），尾部追加一个字段：

```text
value 段: [flag u8][end 字节?][page u8]
page: 请求的批量大小，按条数计（非字节）——
      0 = 引擎默认值（256），0xFF = 「一整页」（旧的缓冲形态）
```

按条数不按字节，因为接收方不得解析 key（约束 4）——字节预算的分页反正要靠尺寸猜测，而条数与发送端迭代器产出的项 1:1 对应。page size 是提示不是契约：接收方可以返回更少的条数（区间结束时就是如此），绝不更多。

### 应答：chunk = 今天的 `OpResponse` + 1 字节尾部

一条流是 `apply` 形状应答的序列，走传输已有的同一关联通道。每个 chunk：

```text
[has_value u8][value LK][value?] [count len-enc] 每条命中: [len][key][len][value?] [tail u8]
tail: 0x00 = 后续还有 chunk（本 chunk 的 count == page size）
      0x01 = 最后一个 chunk（count 任意，可为 0）
```

复用应答语法带来三个性质：

- **chunk 是自足的 `OpResponse` 加一个字节。** 未实现 `OP_SCAN_STREAM` 的接收方拒绝该 op（未知 tag = 畸形帧，`apply` → `None`），发送端退回缓冲路径——不需要版本协商的向后兼容。
- **chunk 携带 value**，顺带修掉 N+1 问题：每条命中是 `[len][key][len][value]`，与引擎的 `(key, value)` 迭代项同构。旧的 `suffixes`-only 应答留给 `OP_SCAN`；`OP_SCAN_STREAM` 永不剥离 value，因为它的契约就是迭代器的契约。
- **接收方零游标状态。** 续传由发送端持有：每个 chunk 的最后一条 key 就是游标，下一次请求是普通的 `OP_SCAN_STREAM`、`begin = last_key + prefix_end 式进位`（经 `[0x02]` 标志字节表示排他 begin）。接收方从「当下」的引擎状态作答——不承诺快照。这对无状态执行器是诚实的契约（约束 2）：远端流是一串「chunk 时刻一致」的读，不是 fjall 本地 `Iter` 那种冻结快照。ADR-0020 的语义恰好在架构本来就要求退化的地方退化。

### 发送端形态：`RemoteStore::scan_range_iter` 成为真惰性

```rust
// 发送端适配器的伪结构
fn scan_range_iter(&self, begin, end) -> ScanIter {
    ScanIter::Remote(RemoteScanIter {
        exec_tx, begin, end,
        page: 256,
        buf: VecDeque::new(),   // 正在耗尽的 chunk
        done: false,
    })
}
// next(): 从 buf 弹出；空且 !done 时请求下一页
// （begin = 上一页末 key + 排他 begin 标志），解码 chunk，按尾字节置 done。
// next_back(): 不覆盖——Remote 臂退化为缓冲，与 slatedb 臂同一规则
// （ADR-0020：引擎给不出的惰性在此丢失；反向的远端遍历需要降序分页
// 请求形状——以同样的理由推迟）。
```

trait 返回类型不变（`ScanIter` 在既有枚举后增加 `Remote` 臂——ADR-0020 的对象安全绕行恰好按设计吸收新后端：加臂是 okm-core 内部事件）。

### 刻意不改变的东西

- `OP_SCAN` 保持原样：`scan`/`scan_suffix` 的消费者是等值形状，缓冲应答就是它们的自然形状；把它们留在流式路径之外，旧语法就少一个活动部件。
- `OP_GET` 保持一元：点查本来就是一条。
- `apply` 签名保持 `Option<OpResponse>`——只是它现在应答的是一页，不是一个区间。主机在调用之间不持有任何东西；「流」是发送端把页缝合起来的结果。
- 无帧级 request id、无确认帧、无接收方游标：关联与重试留在 ADR-0010 §6 安放它们的地方（传输信封与发送端）。

### 被否决的备选

- **帧级流协议（SEQ/ACK 帧、接收方游标 id）。** 违反约束 2（有状态接收方）与约束 3（关联进帧字节）。传输已经保序并关联；在 wire 里重述一遍是同一事实存两次，还会把 codec 耦合到面向会话的传输上——UDS 数据报 / fire-and-forget 形态立即失效。
- **单一应答类型按原始字节片段加偏移流式化。** chunk 不再自足：解析一个 chunk 需要前序 chunk 的上下文，丢一个 chunk 毒化其余。自足 chunk 的代价是一个重复字段，换来免费的续传能力。
- **`next_back` 的降序分页请求（`[0x03]` 反向标志）。** 技术上对称，但当前没有消费者在没正向读过之前反向遍历远端区间；推迟让标志字节空间保持诚实（标志在使用场景出现时加入，不为对称性预支——与 ADR-0016 slot 表的取舍同理由）。

## 后果

- **远端惰性路径同时失去 N+1 与缓冲两个惩罚**：value 随 chunk 走；分页约束每次往返的成本；提前放弃不再拉动后续页。
- **一致性语义显式且弱于本地**：chunk 间无快照。本地引擎给快照迭代（fjall 的 nonce、redb 的事务 guard）；远端路径给每 chunk 一致。必须写进 `RemoteStore::scan_range_iter` 的文档——需要跨远端链路冻结视图的消费者应该物化（一个 Vec）并接受成本。
- **okm-wire 增加一个 op tag 和一个尾字节——零依赖、零语义的宪章保持成立。** hex 测试扩展覆盖：chunk 往返、排他 begin 标志、空尾 chunk、未知 tag 回退。
- **缓冲的 `OP_SCAN` 路径保持为兼容性地板**——早于 tag 4 的对端永远可互操作；发送端按调用发现能力（回退发生在 trait 边界之下，上层无感）。
- **`0xFF = 一整页` 精确保留旧行为**，想要缓冲形态的消费者（`scan_covered` 一类的内部调用方）无感、不被迫。
