# 经由既有 WS 通道集成：在运行中的传输上执行远端 VirtualStorage

> 姊妹篇：[集成扩展类型与原语](EXTENSION-TYPES.zh-CN.md)——FTS/向量/图算法如何落在 OKM 原语上。本篇是传输集成侧：已有 WebSocket 连接（或任何可携带消息的通道）的应用，如何不新开传输就执行远端引擎上的 OKM 操作。

## 前提

OKM 的 remote 后端（ADR-0010）已经把三件事分开：

```text
发送方（RemoteStore）     实现 VirtualStorage，把 ops 成帧
编解码（okm-wire）        手写解析帧：tag + 长度 + 裸字节
接收方（NestStorage）     前置自己声明的前缀，在引擎上重放
```

发送方与接收方之间的传输是后端内部细节：codec 定义「帧是什么」，从不定义「帧怎么走」。一帧就是一个 `Vec<u8>`；任何「一条消息携带一帧」的东西都能当传输。本篇把 WS 场景端到端走一遍。

## 唯一规则：帧是通道的不透明载荷

既有 WS 连接通常有自己的消息协议（JSON 信封、protobuf 事件、realm 消息）。集成规则是对称且严格的：

- OKM 侧**不解析**通道信封——它产出一帧的字节，作为一个不透明载荷交给通道。
- 通道侧**不解析** OKM 帧——它把载荷转交给声明的 `NestStorage`，把返回内容送回去。

按「谁持有连接」分两种形态：

### 形态 A：OKM 在 WS 应用内（常见情形）

应用同时持有 WS 客户端和 `NestStorage`；其它进程里的远端 OKM 实例**经由这条连接**发操作过来。

```text
Krystallizer 进程                         Aura 节点进程
┌─────────────────────┐                 ┌──────────────────────────────┐
│ RemoteStore         │                 │ WS server                    │
│   │ 帧字节           │   WebSocket     │   │ 信封 → 路由：             │
│   ▼                 │ ──────────────► │   ▼                          │
│ okm-wire 编码        │                 │ NestStorage 的泵             │
└─────────────────────┘                 │   ├─ [prefix] 引擎操作        │
                                        │   └─ 应答                     │
                                        └──────────────────────────────┘
```

**接收侧（Aura）**：WS handler 收到信封，按信封约定取出 OKM 载荷，直接调宿主执行核心的唯一入口。`NestStorage` 的执行核心（`NestStorage`——引擎 + 前缀 + `apply`）在 `Arc` 里天然 `Send + Sync`，不需要任何包装。执行是"接收指令、执行、回发"三步，**读写不分岔**：四个操作（put/delete/get/scan）同一帧格式，`apply` 按返回值决定有没有响应——get/scan 返回 Some（响应帧），put/delete 返回 None（无响应，也不需要发）：

```rust
// Aura 侧：宿主构造后，把 Arc<NestStorage> 存进 WS 会话状态
let (host, handle) = AppStorage::serve(engine);
let core: Arc<NestStorage<_>> = nest (Arc) directly;   // 交给 WS handler 持有
host.serve();                                   // mpsc 参考传输照常可开（两者共存）

// WS 消息处理（信封解析是 WS 侧自己的代码，OKM 不参与）：
fn on_ws_message(core: &NestStorage<MyEngine>, envelope: Envelope) {
    // 每个操作帧独立：执行 → 有响应就发回，没有就什么都不发。
    // 信封里的 kind 不区分读写——那不是 OKM 的语义，帧内容自带。
    if let Some(resp) = core.apply(&envelope.payload) {
        send_ws(envelope.reply_to, resp);   // get/scan 的响应帧走同连接
    }
    // 其它应用消息……
}
```

WS 特有的部分（信封解析、路由、请求 id）全在 handler 的解析代码里——OKM 侧给传输的东西就一个 `Arc<NestStorage>` 的一个方法，没有任何需要适配的结构体。mpsc 接线保留为参考传输，与新入口共享同一执行核心，行为零分叉。

**发送侧（Krystallizer）**：发送侧就是正常的存储读写——`RemoteStore` 实现 `VirtualStorage`，`put/get/del/scan_suffix` 四个操作的签名和本地引擎完全一致，调用方感知不到远端。差异被压在实现内部：每个操作编码成帧，经信封送到对端，get/scan 阻塞等响应帧，put/delete 发完即回（fire-and-forget）。换传输（mpsc → WS）只改 `RemoteStore` 内部的发送/等待两个私有方法，`VirtualStorage` 接口和调用方零变化。

注意**什么没有变**：帧字节、codec、host、前缀纪律、batch 原子性映射。发送侧看不到信封——它只看到正常的 VirtualStorage 读写；信封只存在于接收侧 handler 的解析代码里。

### 形态 B：OKM 作为 WS 应用的存储服务

反向嵌入——WS 应用是存储客户端，OKM 跑在自己的进程（或同进程不同 Actor），经由 topic 可达。与形态 A 相同，只是信封两侧角色对调；接收侧 handler 和发送侧 RemoteStore 是同样的两块。

## 可行的信封约定

通道协议需要给 OKM 流量两个槽位：

1. **路由**：帧给哪个声明的宿主（哪个应用/前缀）——一条连接服务多个实例时需要。这是信封数据（topic、路由键），**绝不进帧**：帧自己不知道接收方的前缀（ADR-0010 §5，知识不对称）。裸分片宿主（`NestStorage::bare`，无前缀）对路由到它的每一帧字节原样执行——一个域模型的分片就是编排层 partition-key 路由背后的 N 个裸宿主。
2. **关联**：匹配响应与请求的 id。有没有响应由帧内容决定（apply 的返回值），信封不需要知道读写之分——同一关联机制对两种操作一视同仁。mpsc 靠通道配对免费获得；WS 要么用专门的请求/应答模式，要么按 id 多路复用。两者都是传输策略——帧对此保持沉默。

**不能进信封的**：布局版本、schema 提示、压缩标志——任何让接收方理解帧内容的东西。信封把 OKM 帧当成和其它二进制载荷完全一样的东西对待，这就是隔离机制的全部。

## WS 上的顺序与原子性保证

- **写顺序**：WS 消息按发送序到达（单连接内）；宿主写泵按到达序应用。一帧 = 一次引擎 `commit_batch`，发送方的 batch 原子性 1:1 映射。多条连接打同一个宿主 = 无全序——把每个发送方的写路由到它自己的宿主实例（本来就该每应用一个 `#[kv_nest]`），或接受交错（发送方触碰不相交 key 范围时无害）。
- **读**：任何连接都能服务读；宿主的引擎互斥锁让它们与写泵串行。
- **重连**：WS 断开丢失在途帧（fire-and-forget 写没有 ack）。发送方若需要送达保证，那是通道层的关切（ack 信封 + 重试幂等），不是 OKM 的——OKM 的引擎操作按 key 幂等：重放 put 安全，重放 delete 是 no-op，但 put-delete-put 序列从检查点重放需要检查点在发送方。需求真实时在信封层设计，不要预防性建设。

## 为什么不进 okm-core

WS 适配器是传输胶水：把既有连接绑到宿主入口，加一层信封约定。每个应用的通道协议都不同，所以适配器不可能成为库——它是应用（或 Aura 节点）里的按集成而写的代码，原料是 okm-core 提供的两块：`NestStorage::apply`（接收侧唯一入口）、`RemoteStore`（发送侧，实现 VirtualStorage）。文档级契约就是上面的两个信封槽位。

> English version: [WS channel integration](WS-CHANNEL.md)
