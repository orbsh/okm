# ADR-0078：attach 对一张表在一个 store 上只作证一次，不是每次 open 都作证

**Status**：2026-11-21 裁决，同日落地。PLAN 无对应阶段——这是机制不是功能：`VirtualStorage` 上两个带默认实现的方法、几个能回答的引擎各带一个 `AttachSet`、`attach` 顶部一条快路。前后都量过（1.38 µs → 0.17 µs）。

`Collection::open` 就是 attach（ADR-0074：只有一个 fallible 构造函数，句柄存在之前表必须先对它的 store 校验过）。校验做的四件事是：点读 meta 行、解码存储里的声明、与编译进来的声明比对、比对 capture 标记。实测 warm fjall 上每次 open 要 1.38 µs，而同一次点读只要 0.09 µs——所以价钱在比对，不在 I/O。而比对所要保护的东西，在一个运行中的二进制里一样都不会变。

## 背景

三条事实决定"这个 store 是否已经作证过这张表"这个答案能放在哪里。

声明是编译进来的。`CollectionSchema::of::<K, R>()` 是声明的纯函数，`R::PLAN` 每进程填一次、地址固定。运行中的二进制不存在"声明变了"这回事。

行不会被写入者之外的人改。fjall 目录与 redb 文件都拿排他锁，ephemeral 是每个句柄一份。store 写下的那一行就是 attach 之后会读到的那一行，所以本进程里成功过一次的校验，不会被同一个 store 后来的读取推翻。

但作证是按 store 的，plan 是按 ns 的。`__OKM_NS_{ns}_ENTRY` 以 ns 号为键、被进程里所有 store 共用（`#[ok_shared_ns_in_tests]` 存在的意义就是弯掉它）。"这个 ns 校验过了"不是这样：那是一个 store 的一行的事实。把两者合一——把作证塞进 plan 静态——第二种情况立刻出错：第二个同类 store 会被判成已作证而跳过种自己的行，而一行都没有的表，对任何没有声明可比对的读者都是不存在的。每个测试进程都会用同样的类型打开很多 store；服务换个目录重开也是一样。

所以作证需要 (store, 表头) 这把键——表头是 `[partition][ns]`，因为两个分区就是两张表——唯一的问题是这把键放在 store 里，还是放在进程级的 store 表里。

## 裁决

让 store 回答。`VirtualStorage` 加两个方法，默认实现完全保留今天的行为：

```rust
fn attached(&self, _header: &[u8], _decl: &'static str) -> bool { false }
fn mark_attached(&self, _header: &[u8], _decl: &'static str) {}
```

键是表头**加**声明。只用表头是第一版草稿，被现有测试当场撕掉了：`attach_test` 会在同一个 store 上打开一个 `#[ok_shared_ns_in_tests]` 双胞胎，它与前者共用表头，于是它的 open 直接走快路、从未被比对——它存在就是为了验证的那个拒绝消失了。声明用 `std::any::type_name::<R>()` 而不是 `TypeId::of::<R>()`，因为 `Document` 不要求 `'static`；这把键只需要在一个进程内唯一。

`attach` 先拿两半键问一句。命中就把 plan 静态填热（每次提交都读它，而它由声明决定），然后返回——不读、不比、也不调 `note_table`：同一个 store 实例在第一次作证时已经被通知过了。未命中就走原来的完整路径，`mark_attached` 放在**最后**，在行写下、store 被通知之后，所以失败的 attach 不留任何主张。

引擎用 `Arc<AttachSet>` 声明参与——一个互斥量后面装着表头的小 `Vec`，线性扫描：表头两到四字节，一个 store 有几十张表而不是几千张。用 `Arc` 是因为克隆就是同一个 store（fjall 的 `Database`/`Keyspace`、redb 的外层 `Arc`、ephemeral 的内层 `Arc`）：一个克隆作证过，它的兄弟们必须看得见。`&S` 转发。`ChangelogStore` 不加集合——它本来就有 `header → Known` 给 `note_table` 用，作证就是在那张表里命中一次。

回答不了的 store 保持旧行为：`RemoteStore`（一条通往工作线程的通道，本地没有行的视角），以及本 crate 之外写的引擎。`false` 永远正确，只是要付那一次读。

## 换来了什么

同一台机器、release、warm store 上各 20,000 次 open，前 → 后：

```
fjall open, warm        1.38 us  ->  0.17 us/op
ephemeral open, warm    1.66 us  ->  0.18 us/op
fjall get_row (miss)    0.09 us      0.10 us/op   （未变，作为尺度）
```

一次 open 现在约等于一次点读，而不是一次点读加一次声明比对。一张表在一个 store 上的第一次 open 仍付全价——需要写行时 43.8 µs，只需读行时 1.4 µs——那才是该落在"让表存在"那条路径上的成本。

## 被否

**作证放进按 ns 的 plan 静态。** 作用域错了：静态被进程里每个 store 共用，第二个 store 就永远不会种自己的行。`OnceLock` → `LazyLock` 解决不了——两者都答"每进程算一次"，而那正是 bug 不是修法。

**只用表头作键。** 一个 ns、一张表、一个声明——生产里成立，守住那个拒绝的测试里不成立：`#[ok_shared_ns_in_tests]` 双胞胎按构造共用表头，只按表头作键就等于把它送进快路，必须拒绝它的那次比对永远不会跑。共用一个表头的声明，就是每次都得比对的声明。

**进程级、按 store 身份索引的全局表。** 它要给每个引擎一个稳定 id（计数器、指针、路径），要给测试进程制造的成千上万个临时 store 一个交代，换来的还是和 store 内部集合一样的 1.2 µs。查找从来不是成本：扫三个字节是几十纳秒，对手是 1.4 µs。

**把 `Collection` 变成 `Sync`，让调用方持有一个句柄而不是每次 open。** 这是消除每次 open 成本的另一条路，但撬错了杠杆：批次计数背后的 `Cell` 是**故意** `!Sync` 的（ADR-0035 §10）——merge 的读-改-写需要 `&self` 写不再从 `&mut self` 得到的排他性。把它换成锁，等于为省一次 open 的 1 微秒把争用搬到写路径上。想只 open 一次的调用方可以按线程持有句柄。

## 后果

一张表在一个 store 上的第二次 open 不再校验任何东西。这正是要点，而且只在"一个 store 一个写者"下成立；若另一个进程在生命周期中重写同一个 store 的字节，本进程会继续使用第一次 attach 时校验过的声明。这对这里每个引擎都不成立（fjall 与 redb 有锁，ephemeral 每句柄一份），而且 `note_table` 缓存它读到的那一行，早就是这个口径。

同一进程里两次 open 之间被换掉布局的表不会被第二次 open 发现。今天 okm 里做不出这种事：布局变更是一次迁移（ADR-0075）——它把表搬到另一个奇偶下、靠一次静默翻转，在此之前旧代服务全部读，没有哪个句柄被原地改指向。