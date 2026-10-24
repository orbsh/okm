# ADR-0039：存储 trait 的写动词返回 `Result`

状态：Accepted（2026-10-24）。ADR-0037 的后续——那篇给了写动词错误通道，并点名它留下的两个闭合面；本篇打开存储 trait。读侧的半边（损坏字节）按设计继续闭合。

## 背景

ADR-0037 让每个写动词都可失败，并记下了它刻意没碰的东西："逐条底层存储方法（`VirtualStorage::put` / `del`）——两者都是'存储不是它声称的样子'，……拓宽任何一边都是另一个题目（完整性报告、存储 trait）。"

如今存储 trait 是调用方的 `Result` 唯一的断头处。自 ADR-0038（"读失败不是行不存在"）起 `get` 与所有 scan 都返回 `Result`，后端有地方放读的拒绝；而 `put` 与 `del` 仍返回 `()`，同一个后端无处放写的拒绝——引擎侧的证据就在代码里：`slatedb_backend` 用 6 处 panic/`expect` 回答写错误，`redb_backend` 8 处，`fjall_backend` 2 处，`localstorage_backend` 2 处，两个发送端（`nest` 的 `RemoteStore`、`WireClient`）在通道关闭或连接已死时不声不响地丢掉写入。

这些位置掩盖的失败，恰好是调用方**能**处理的那两类：

- 存储拒绝了这次提交（I/O、写者被 fence、配额满、存储已关闭写入）；
- 传输没了，这一帧永远不会发出（`WireClient::fail` 早已为查询记下 `dead` 原因——写入忽略它）。

两者都不是"存储不是它声称的样子"。损坏的 payload 是；被拒绝的写是存储在说不，而写者正是必须听到这句话的一方。

## 决定

1. **`VirtualStorage::put` / `del` 及异步孪生返回 `crate::error::Result<()>`。** `Err` 是 `Error::Engine(text)`，内嵌后端自己的原文。`put` 的 `Ok(())` 表示存储**已经有**这串字节（不只是调用被接受）；`del` 的 `Ok(())` 也覆盖"本来就没有"——缺席不是失败，与 `get` 的 `Ok(None)` 同一条规则。
2. **仓内每个后端映射自己的原生错误**，写路径上不留 `expect`：`redb` 复用它的 `eng` 映射（begin_write / open_table / insert / remove / commit），`fjall` 映射 `insert` / `remove`，`localStorage` 映射 `set_item`（配额拒绝就是浏览器说不）/ `remove_item`，`slatedb` 的异步实现等待 durable handle，所以 `Ok` 意味着跨过了 WAL 边界——与它自己 `commit_batch` 覆写早已遵循的规则一致。
3. **slatedb 的同步路径变成一次往返。** `Cmd::Put` / `Cmd::Del` 增加 reply 通道，`put_sync` / `del_sync` 回答引擎的裁决而不是往虚空里投递；驱动线程等待 durability 后回答，而不是在驱动线程上 panic——那里没有任何调用方看得见。通道顺序本来就保证后续读能看见这次写，所以代价是一次 `recv`，不是调度形态的改变。
4. **`RemoteStore::send_write` 返回 `Result`**：executor 通道关闭意味着 nest 没了、帧丢了；`commit_batch` 传递它而不是 unwrap。
5. **`WireClient` 报告自己那一半。** 连接已死或 `post` 被拒都是 `Err`——此前两者都是静默丢弃，使丢失的写与已提交的写不可分辨。投递保证本身仍归传输层（ADR-0010 §6）：客户端只报告帧已离开，不报告对端已应用。
6. **`BatchStore` 保持不可失败**（`Ok(())`）：把一条 op 缓进 `Batch` 不会失败，`commit_batch` 才是引擎回答的地方。
7. **`AsyncJunction::link` / `unlink` 可失败**，与 ADR-0037 已改的同步 `Junction` 对齐。

## 否决的备选

- **保留 `()`、后端里 panic**（此前状态）。服务用 `put` 写就死在同一个存储错误上，而走 `merge` 时那只是一个返回值——这是 ADR-0037 的论证下移一层；层边界并不改变谁能处理这个错误。
- **延迟错误槽，由下一次 `get` 报告。** 失败的写会以另一个操作的错误出现，归咎于错误的调用方；只写不读的嵌入永远学不到；而一个批会为 N 条 op 报一个错误，且说不出是哪条。
- **保留 `put`，另加 `try_put`。** 一个操作两个动词是在邀请静默路径——正是 ADR-0037 否决的"半 Result"形状。
- **让 `bindings/` 里宿主注入的 `Engine` trait 携带 `okm_core::Error`。** 那个 trait 是刻意不携带任何 okm 类型的 FFI 契约（"bytes in, bytes out"、与版本无关）：宿主可能持有另一个 okm 构建，其失败类型不是我们能命名的。它改为回答 `Result<_, String>`（已落地的形态），适配它的 box 把字符串映射成 `Error::Engine`。

## 后果

- 每个后端的写路径都能在类型上拒绝：slatedb 6 处（异步 put/del、同步 put_sync/del_sync、驱动的两个臂），redb 8 处，`fjall` 2 处，`localStorage` 2 处，`nest` 的 `send_write` 与 `commit_batch`，以及两个 wire 发送端。
- slatedb 的同步写现在等待 durability 与驱动的回答。这是同步路径上的一处延迟变化——换来的正确性是 `Ok` 表示已提交，而这本就是其它同步引擎（redb 的 commit、fjall 的 insert）的含义。
- `bindings/*` 不是 workspace 成员，任何 workspace 的 check 或测试都看不到它们。宿主注入的那个缝现在是 `Result<_, String>`（提交 `38c5c6c`，ADR-0037 §1）：okm 的错误类型不得越过该边界，纯字符串可以。那里当时仍然开着的是各绑定自身的函数体——29 处 `unwrap`/`expect`，其中 15 处是与锁中毒那一笔只隔一行、写法跨行的同一个模式。两轮都已落地（`08c961e`、`9f2ee32`），这些函数体现在是零；剩下的是要不要给这三个独立 workspace 各自的 `[lints]`——独立 workspace 不会继承根 workspace 的门禁。
- **`Error` 标了 `#[non_exhaustive]`**：本 ADR 与 ADR-0038 各自都是同一个枚举上多出的一条臂，所以下一条必须按构造就是增量的——新变体对下游必须是编译期无感。从别的 crate 匹配 `Error` 要多写一个 `_ =>` 臂，这是"以后还能答一种新失败"的代价。整个家族里没有谁穷尽匹配过它，所以这个属性落地时没有任何代码需要动。
- **读臂同一轮一起改了**：slatedb 驱动的 `Get`、`ScanRange`、`ScanSuffix`、`ScanStream` 都经通道回答 `Result<_, String>`（`SlatedbSync::request` 返回 `crate::error::Result`；开不了范围的扫描交给调用方一个「唯一一项就是该错误」的迭代器），redb 的 `scan_range_iter` 同法——错误挂在 **item** 上（ADR-0038：流式构造函数无法替它回答）。`localStorage` 读侧已无生产代码 panic。引擎层剩下的只有构造与不变量：`SlatedbSync::open_mem` 的 runtime/线程创建（只跑一次的两处 `expect`）、`nest` 的 executor 不变量（exec_rx 锁、serve 两次、engine 锁、begin present）、`test_engine` 的 open 助手。
- **被拒绝的写会以 `Err` 回来，且有测试锁住**（同一轮）：`write_refusal_test::a_refused_write_comes_back_as_the_engines_text` 用一个拒绝写入的 store 走 `Collection::put`，断言引擎自己的原文；`wire_client_test::a_write_on_a_dead_connection_answers_err` / `a_write_whose_post_fails_answers_err` 覆盖发送端那一半。**仍未覆盖**：对原生引擎本身做故障注入（逼 redb / fjall / slatedb 拒绝意味着破坏一个活着的存储；它们的映射是一行 `map_err`，没有可写错的分支），以及 nest 的"executor 不在"臂——只要有发送端活着它就触发不了，保持为防御臂。
- **默认的 `commit_batch` 在拒绝下不原子。** 它逐条重放 `put`/`del`，所以引擎拒绝第 N 条时，第 1..N 条已经落地——只有带原生 batch 的引擎（fjall、redb、slatedb）才是全有或全无。`a_replayed_batch_leaves_the_ops_before_the_refusal` 锁住这条边界。这也解释了为什么 ADR-0037 的"被拒绝的写什么都不写"对**校验**类拒绝成立（它们发生在任何 op 进入 batch 之前），对引擎拒绝不成立。
- **写现在会应答，所以写路径上的引擎 panic 也看得见了。** nest 的泵把每一帧包在 `catch_unwind` 里；写者原先不携带 reply 箱（ADR-0010 §6），写路径上的 panic 看起来像成功。这一项在同一轮里落地：每个合法帧恰好答一次（写帧的应答是空 ack，拒绝是 `fault`），两个发送端都等这个应答，泵把捕获到的 panic 变成 fault，而不是让调用者永远挂着。代价是 fire-and-forget 写形态——写变成了一个往返。
- 下游迁移是机械的，也正是这次作为一次 API 断裂发布的原因：调用方本来就返回 Result 的地方补 `?`，测试里补 `.unwrap()`（ADR-0037 的迁移规则）。