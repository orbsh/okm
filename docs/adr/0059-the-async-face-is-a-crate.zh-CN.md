# ADR-0059：异步面自成一个 crate，不引入就等于不存在

**状态**：已接受并落地（2026-11-10）——`okm-async` 已存在，core 不再有 `slatedb`、`test-engines` 与 `tokio`，同步适配器交回的是 `ScanIter::External`。工作区绿：646 个测试、clippy 干净。
**日期**：2026-11-10
**中文**：[0059-the-async-face-is-a-crate.zh-CN.md](0059-the-async-face-is-a-crate.zh-CN.md)
**依赖**：ADR-0020（扫描是惰性的）、ADR-0027（存储面审计）、ADR-0028（线上发送端是客户端，不是存储）
**相关**：ADR-0010（远程后端只搬字节）

## 背景

core 的存储契约一直是同步的：`VirtualStorage`（`okm-core/src/engine/storage.rs:28`）只有 `put` / `get` / `del` / `scan_suffix` / `scan_range` / `scan_range_iter`，一个 `async fn` 都没有。异步面是另一件小事：`VirtualStorageAsync`（`:377`），六个方法的镜像，**实现者恰好两个**——`SlatedbStore`（引擎本身是异步的）与 `WireClient<T>`（传输用 future 应答的发送端）。`SlatedbSync`（`slatedb_backend.rs:596`）把异步引擎压回同步契约：它只持一个 `mpsc::Sender<Cmd>`（`:152`），一个线程里跑 `drive(rt, db, rx, durable)`（`:164`），线程内带一个 tokio runtime，每次调用等一条回复。

所以异步面存在的原因有两个，代价也分两处，而源码里只看得到一处：

- **运行时。** `tokio` 进树只有一条路径：`slatedb = ["dep:slatedb", "dep:tokio"]`（`okm-core/Cargo.toml:29`）。`WireClient` 不需要任何 runtime——`wire_client.rs` 是手写的 `Future` / `Waker` / `Poll`，基于 `std::future`，与 tokio 无关。
- **边界。** 其余一切都在 core 里靠 feature 关着，而 feature 不是边界：cargo 对整张构建图统一 feature，任何一个 crate 打开 `okm-core/slatedb`，同一张图里所有用 `okm-core` 的构建都会把 `tokio` 编进来。core 自己的 `test-engines = ["slatedb"]`（`:28`）正是这么干的，所以每一次 `cargo test --workspace` 都带着 tokio 与 slatedb，不管调用方要不要。

裁决当日的清点（2026-11-10）：

- `WireClient` 在仓外没有消费者，只有 `okm-core/tests/wire_client_test.rs` 用它。线上的远程形态是 mudra 的 `NestStorage<FjallStore>` 收 WS 帧，以及 aura 把 `okm_wire::OpFrame` 应用到同步 store 上。
- `TestStore`（`engine/test_engine.rs`）是真实后端上的矩阵枚举——slatedb 内存态（零文件系统，默认）、fjall 临时目录、redb 临时文件——其中 slatedb 那一臂就是 `SlatedbSync`。它的使用面：`okm-core` 48 处、`okm-lake` 13 处、`okm-dynamic` 8 处、`okm-query` 1 处、两个 bindings 共 3 处。
- `~/world` 下没有任何 crate 提到 `SlatedbStore`、`SlatedbSync` 或 `WireClient`。

## 裁决

**异步面以 `okm-async` 出货，core 不再持有它。** 这个 crate 装：

    VirtualStorageAsync        镜像面，连同默认方法与 async_fn_in_trait 的 allow
    WireClient / WireTransport  在注入的字节传输上发送的异步客户端
    SlatedbStore / SlatedbSync / SlatedbIter，以及打开 DB 的辅助函数
    TestStore                  引擎矩阵——它是真实后端上的枚举，其中一臂是 slatedb，
                               那一臂在的地方它才能存在

依赖只有一个方向：`okm-async → okm-core`（要 `Batch`、`ScanIter`、`Error`）加 `okm-wire`；core 里没有任何一处提到它。core 留下整套同步契约——`VirtualStorage`、`Batch`、`ScanIter`、`SharedVirtualStorage`、`NestStorage`（接收端）、`RemoteStore`（mpsc 传输上的同步发送端，`nest.rs:122`），这些没有一样是关于异步的。

**core 只多一个变体：`ScanIter::External(Box<dyn DoubleEndedIterator<Item = Result<(Vec<u8>, Vec<u8>)>> + Send>)`。** `ScanIter`（`storage.rs:174`）是封闭枚举，各臂由特性门控（`Fjall`、`Redb`、`Slatedb`、`Remote`、`Buffered`），它的 `Iterator` / `DoubleEndedIterator` 实现也在 core（`:192`、`:219`）。`Slatedb` 那一臂跟着 crate 走，而另一条路——让同步适配器先把整个区间缓冲下来——删掉的不是优化而是保证：ADR-0020 把扫描定成惰性，正是为了让半途放弃只付已读的代价。类型擦除的那一臂与引擎无关：它是任何住在 core 之外的引擎交回迭代器的口子，于是下一个搬出去的 crate 不必再改 core。

**`test-engines` 不再是一个特性，而是 dev-dependency**，出现在需要矩阵的各个 crate 上。cargo 允许这个环（`okm-core` dev-dependency 指 `okm-async`，而 `okm-async` 依赖 `okm-core`）。`okm-core` 自己的 dev-dependencies 保留 `tokio`，因为它的测试直接用；那是只属于测试的边，不在契约里。

**边界由构建来守，不靠记忆。** CI 断言 `cargo tree -p okm-core`（正规依赖边）里既没有 `tokio` 也没有 `slatedb`，并且 `okm-core/src` 下不出现 `slatedb` 字样。

## 为什么切的是异步，不是后端

fjall 与 redb 留在原地、留在特性后面，这个不对称是对的，不是遗漏。按后端一刀切，切在什么都不变的地方：`FjallStore` 与 `RedbStore` 是一个同步字节 KV，从契约这一侧看可以互换，差别只在常数（内存还是文件、迭代形状）与一条依赖边上。`slatedb` 的差别是类型系统看得见的：它实现的是**第二个 trait**，需要一个 runtime，还需要一个适配线程之后同步契约才驱动得动它。

切分的单位应该等于变化的单位。这里变化的是契约——一个带惰性有序扫描的同步字节 KV——所以契约留在 `okm-core`，凡是说另一种契约的都去 `okm-async`。这个 crate 按轴命名而不是按产品命名：下一个异步引擎直接加入，将来的异步 sink 或 watcher 也有明确去处。

crate 是**删除的单位**，不是契约的单位。slatedb 今天是受支持的后端，而"哪天不想要了"由一条目录回答：删掉 `okm-async`，去掉那几行 dev-dependency，`ScanIter::External` 那一臂可选（它与引擎无关，没人用也无害）。core 里没有任何东西需要拆解，因为 core 从来没有和它纠缠。这就是本裁决买到的东西，也是"要不要保留 slatedb"今天不必有答案的原因。

## 否决

- **`okm-slatedb`——每搬走一个引擎就开一个 crate。** 结构一样，轴错了：按产品命名的 crate 会让**下一个**异步后端只能在"加入一个以竞争对手命名的 crate"与"再开一个"之间选。相对 fjall/redb 的不对称是真实的；用其中一个引擎的名字命名，只是把这个不对称藏起来而不是说出来。
- **维持现状，全都留在 core 的 feature 后面。** 它恰好在"删除"这个需求上失效：变体、适配器、特性、测试互相穿插，摘掉 slatedb 等于对 `storage.rs`、`test_engine.rs`、`lib.rs`、`engine.rs` 和测试矩阵做手术；而且 feature 统一会把 tokio 塞进没人要它的构建。
- **`VirtualStorageAsync` 留在 core，只搬引擎与客户端。** 少搬一个文件，却更差：同一份契约的两半分处两个 crate，靠约定互相对齐，没有任何机械检查能证明它们仍然同形。今天它们相邻在一个文件里，审计只是抬眼一看。trait 跟着它的实现者走。
- **让同步适配器缓冲、跳过 `ScanIter::External`。** 这能让 core 一处都不改，代价是 ADR-0020 定下的惰性：对一百万行的区间做 `LIMIT 10`，会先把整个区间物化出来。动态派发是这笔交易里更便宜的一侧。

## 放弃的东西

- **相邻性。** 镜像面与被镜像的 trait 不再在同一个文件里；ADR-0027 的「两个面保持同形」从编译器与读者一眼可见，变成一条评审纪律。
- **一个开关下沉而不是消失。** `okm-async` 内部仍需要 `slatedb` 特性（默认关）：镜像面与 `WireClient` 不带 runtime，`SlatedbStore` 带 tokio。只想要 trait 的人不该顺带拿到引擎。
- **每次取项一次虚调用**，用于 core 之外引擎的扫描（`ScanIter::External`）。常数级，而且这条路径在适配器里本来就要过一次信道往返。

## 后果

要搬的文件：

    okm-core/src/engine/slatedb_backend.rs   → okm-async/src/slatedb_backend.rs
    okm-core/src/engine/wire_client.rs       → okm-async/src/wire_client.rs
    okm-core/src/engine/storage.rs           VirtualStorageAsync 搬走；ScanIter 增加 External
    okm-core/src/engine/test_engine.rs       → okm-async/src/test_engine.rs
    okm-core/tests/slatedb_eval.rs           → okm-async/tests/
    okm-core/tests/wire_client_test.rs       → okm-async/tests/

core 要改的地方：`Cargo.toml`（去掉 `slatedb` 与 `test-engines` 两个特性、`slatedb` 与可选 `tokio` 两个依赖，以及点名它们的 `required-features`；加 dev-dependency）、`engine.rs`（模块声明）、`lib.rs`（`SlatedbStore`、`slatedb_backend`、`WireClient`、`WireTransport`、`test_engine` 这几处再导出）、`storage.rs`（`External` 加两个 match 臂），以及删掉所有 `ScanIter::Slatedb` 的使用处。

测试面的扩散：`okm-core`、`okm-dynamic`、`okm-lake`、`okm-query`、`bindings/okm-entry`、`bindings/okm-python` 各加一条对 `okm-async` 的 dev-dependency，并把导入处的 `okm_core::TestStore` 改成 `okm_async::TestStore`——机械改动，约 70 处调用，行为不变。

下游不动：okm 之外没有任何 crate 用到这个 crate 带走的那几个类型。

纸上看不到、落地才付的一笔代价：**`okm-core` 自己的单元测试用不了 `okm_async::TestStore`**。一个会指回本包的 dev-dependency 会让 cargo 把这个包编译两遍——一遍给测试、一遍给依赖边——两份的 `VirtualStorage` 是不同的类型，矩阵永远满足不了那个 bound。集成测试没有这个问题（那里 lib 是依赖而不是被测单元），继续跑真实引擎。所以 core 的单元测试跑在 `engine::mem_store::MemStore` 上：`BTreeMap` 加一把锁，`#[cfg(test)]`，约五十行。它不覆盖引擎行为——异步适配器的线程与信道、fjall 的快照语义——那些正好在 `okm-async` 自己的测试与集成矩阵里。

按证据重开，而不是按口味：

- slatedb 不再是受支持的后端 → `okm-async` 只剩镜像面与一个无人认领的发送端，那时诚实的做法是删掉这个 crate，而不是留个占位；
- `WireClient` 在 okm 之外有了真实消费者 → 这个 crate 有了不依赖任何引擎的存在理由；
- 第二个异步引擎出现 → 这个 crate 正好在做它名字所说的那件事。

## 历史

1. 2026-11-10 —— 起初的题目是「把 SlateDB 从事务引擎里踢出去，让存储契约回归同步」。读代码后这个前提没站住：`VirtualStorage` 里没有 `async fn`，runtime 进入的唯一入口是 `slatedb` 特性。问题于是从"契约是否同步"变成"异步面住在哪里"。
2. 先叫 `okm-slatedb`，后改 `okm-async`：切分轴是契约，不是产品。
3. `TestStore` 本来打算留在 core，是 slatedb 那一臂逼出了这个问题；它跟着 crate 走，dev-dependency 的扩散也由此而来。