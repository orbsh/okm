# ADR-0028: 浏览器内的 VirtualStorage 家族——localStorage 引擎 + 异步线协议客户端

> **Languages:** [English](0028-in-browser-virtualstorage.md)（主文档） · [中文](0028-in-browser-virtualstorage.zh-CN.md)

**状态：** Implemented（8d00132 + 8a12f81）。**2026-11-10 修订：`LocalStorageStore` 一半已
撤除**——那个同步浏览器引擎、它的 `localstorage` feature 与 `web-sys` 一起离开了 `okm-core`；
线协议客户端那一半保留。见「撤除」一节。

## 背景

mudra R3（mudra `PLAN.md` §11）在浏览器里放了两个存储角色：

1. **面板本地 UI 态**（树展开、排序偏好、草稿）——类型层
   （`Collection`/`Graph`）绑同步 `VirtualStorage`，所以这个角色需要
   真正同步的浏览器存储。localStorage 是唯一 API 同步的（
   `getItem`/`setItem`/`removeItem`）；IndexedDB 天生异步，与同步 trait
   结构不合。
2. **远端业务数据**——面板已手写了一个 RemoteStore-over-WS（mudra
   `crates/mudra-panel/src/remote.rs`）：一条 WS 消息 = 一个 `okm-wire`
   帧、查询帧与响应 FIFO 配对、写帧 fire-and-forget（更正：写现在会答 ack，
   共享客户端每帧都配对——见 ADR-0039 里已落地的后续项）。这个手写面板客户端
   已退役；它曾是补齐后异步面
   （ADR-0027）的第一个生产消费者；R3 的问题是：把这个模式上收成 okm
   的共享线协议客户端。

形态分裂是结构逼出来的，不是选型：`RemoteStore`（同步，ADR-0010）阻塞在
`mpsc::recv()`——浏览器里没有可阻塞的 runtime（即便 native，重入
`block_on` 也 panic，见异步模块头注释），同步 WS 发送者结构性不可行。
所以浏览器拿到**两个不同 trait 的客户端**：同步引擎（localStorage）+
异步线协议发送者。

## 决策

1. **`LocalStorageStore`，okm-core feature `localstorage`**——对
   `web_sys::Storage` 实现**同步** `VirtualStorage`：
   - `put`/`get`/`del` → `setItem`/`getItem`/`removeItem`；字节以 base64
     跨界——ADR-0018 的例外条款在此精确适用：宿主 API 物理上只装字符串，
     base64 是边界产物。
   - `scan_range` → 枚举全部 key（`length` + `key(i)`）、解码成字节、
     过滤 `[begin, end)` 区间、**在 Rust 侧排序**——JS 字符串序是 UTF-16
     码元序，不是 UTF-8 字节序，直接比较枚举出的字符串会把 U+FFFF 以上
     的 key 排错。O(n) 枚举扫描在 UI 态规模（千级行、数 MB 配额内）可接受。
   - `commit_batch` → trait 默认逐条重放（无引擎级原子性——诚实标注的
     边界：UI 态容忍部分写；不能漂移的数据走拥有它的节点）。
2. **`WireClient<T: WireTransport>`，okm-async，实现 `VirtualStorageAsync`**（ADR-0059 把异步面整体搬出 core；trait 与发送端同住一个 crate）
   ——面板 RemoteStore 的通用形态：
   - 传输**注入**（`fn post(bytes) -> Result`），不 import 平台：客户端
     永远不知道自己骑 WS 消息、UDS 数据报还是进程内 pump。接收侧 intake
     早已传输无关（`NestStorage::apply`）；这一条把发送侧做成对称。
   - 调用方把入站响应喂给 `deliver(bytes)`；客户端**按 FIFO 序唤醒
     waiter**——合法性来自接收端对单连接帧的顺序 apply（收序=发序），
     且写帧永不挂 waiter（按协议无响应）。
   - waiter 手写纯 std oneshot cell（waker 注册），不引 tokio——该
     feature 单独开启时 crate 必须可 wasm 构建。
   - 传输失败对未决 waiter 显式 `Err`（绝不静默悬挂）——mudra 面板的
     断线规则，上收为通用。
3. **`VirtualStorageAsync` 从 `slatedb_backend.rs` 迁入 `engine/storage.rs`**
   ——补齐后的对齐面是引擎契约（ADR-0027），不是 slatedb 的属性。异步
   trait 转为无 feature 依赖（AFIT，`#[allow(async_fn_in_trait)]` 的理由
   随迁）。
4. **类型层不动**——`Collection`/`Graph` 仍绑同步 `VirtualStorage`；
   浏览器类型数据走 `LocalStorageStore`，远端访问经 `WireClient` 说裸
   字节。类型语义留节点侧（ADR-0010 为远端后端划的同一条边界）。

## 考虑过的替代方案

- **IndexedDB 后端**——否决：异步出身满足不了同步类型层，而对异步线
  协议客户端它只是无消费者的第二种形态。localStorage 是 R3 已写的落点。
- **`block_on` 实现同步 `VirtualStorage` over WS**——否决：结构性不可行
  （见背景），wasm 主线程永挂。
- **面板 RemoteStore 原样保留、不上收**——否决：FIFO 配对、断线显式
  Err、写帧静默，是第一个消费者发现的线协议**契约事实**；留在 mudra
  等于下一个 wasm 消费者重新实现并重新发现一遍。R3 已定落点：okm，
  随其测试纪律。
- **客户端握手 / 线协议版本头**——否决：帧不携带语义，布局版本化是发送
  方进程内的事（既有复用契约决定；勿再提案）。

## 边界

- `LocalStorageStore` 只用于浏览器本地态。业务写保持单控制点：保形
  变更走拥有节点的界面（mudra：8899 动词），绝不经客户端直写。
- FIFO 配对的前提是**单连接 + 顺序 apply 的接收端**；多并发连接各自
  一个 `WireClient`。
- `WireClient` 的 `scan_range_iter` 暂用异步 trait 的缓冲默认；宿主需要
  时的第一根性能杠杆是分页 `OP_SCAN_STREAM` 覆写（chunk 语法早已上线，
  ADR-0021）。

## 消费者

mudra R3：面板 `remote.rs` 迁移到 `WireClient`（它手写的配对逻辑成为
crate 的测试基线——以 native 内存传输套件移植进 okm）；面板本地态
（`filters`/`collapsed`/`sortNew`，现为内存 signal）在 `localstorage`
feature 后接 `LocalStorageStore`。受 mudra 离线判据门控：该 feature 开启
状态下 `cargo check --target wasm32-unknown-unknown` 绿。

## 撤除（2026-11-10）：同步浏览器引擎

`LocalStorageStore` 已经删掉了。上面记的是它当年要做什么，保留原样；下面是为什么是删它、
而不是别的做法。

**它唯一的消费者只存两个键的 UI 态。** mudra 面板持久化的是 `filters` 与 `collapsed`——几十
字节、尽力而为、缺席时用默认值。为一个两个 blob 的缓存去实现完整的 `VirtualStorage`（扫遍该源
下所有键、批量、远端句柄）是用错了工具；面板现在直接拿 `web_sys::Storage`，三十行左右，字节
形状不变。

**core 的依赖闭包是一条被检查的边界。** ADR-0059 把 tokio 与 slatedb 从里面拿走并加了 CI 检查，
理由是：一个把异步运行时和对象存储带在身上的存储库，是每个消费者都要付账的存储库。`web-sys`
是同一类杂质，只是换了一层——而且与本地的那些引擎不同，这个后端在本仓里一个测试也跑不了：它
唯一的目标是 `wasm32`，而唯一可测的部分是 base64 那道跨界。

**这套论证不授权的东西。** 一个浏览器内的**引擎**仍然是合理的需求——整个数据集就活在页面里
的应用。那个引擎不可能是 `VirtualStorage`（core 是同步的，而有持久性的浏览器存储都不是），
所以它归 `okm-async`，与 `SlatedbStore` 并列，实现 `VirtualStorageAsync`。纸上最合适的候选是
SurrealKV（点读与迭代同步、提交流水线异步、Apache-2.0、CI 里给 `wasm32` 建过），等真有这样的
消费者出现时再评估，并带着它自己 README 写明的约束：`Tree` 必须挂在**专属 Web Worker** 上、文件放 OPFS，同一时刻只有一个 worker 持有某个数据库，wasm 只是 Tier 2（建过、没在本机跑过测试）。它的链接体积、Tokio 运行时的硬要求与其余实测记在 ADR-0064（那里把它定为暂缓）。
这些都不改变本次撤除：UI 态缓存不是把一个 LSM 引擎塞进面板的理由。

**保留的东西。** 这份 ADR 的另一半没有动：`WireClient` + `WireTransport` 仍是浏览器用来处理
一切「非本地」的异步面，面板的远端路径仍然走它们。
