# ADR-0035：整文档只有一个接口面 —— `view` / `merge` / `replace` / `purge`

**状态**：Accepted（2026-10-22）——与本文一并落地
**日期**：2026-10-22
**英文**：[0035-whole-document-api.md](0035-whole-document-api.md)

## 背景

document API 是按**存储布局**切的，不是按调用方的意图切的：槽 0 是 `put` / `get` / `delete`，槽 1 是 `get_fields` / `put_fields` / `delete_fields`，整文档写 `put_document` 靠名字在两槽之间路由。由此三个后果。

1. **调用方必须知道布局。** 一个名字落在声明结构体还是动态段，是存储事实。手里拿着外部 map 的调用方，得先知道自己的 schema 才知道该用哪个入口。
2. **整文档写要两次提交。** `put_document` = `put` + `put_fields`，即两次引擎提交，两槽在崩溃点会撕开 —— 这个操作对自己都不原子。
3. **`put_document` 的名字在说谎。** 它叫 put，行为是 merge：map 没点名的声明字段保持库里的值。API 里从来没有一处说"这张 map 就是文档"。

实现时还有一条观察：typed 侧根本没有单字段 setter（derive 不生成），所以"改静态段一个字段"本来就已经是整结构体的读-改-写。整结构体替换是唯一的 typed 写。

## 决策

接口面按**值的形状**切，而不是按槽切。

| 操作 | 覆盖 | 提交 | 取代 |
|---|---|---|---|
| `get(&key) -> Option<R>` | 静态段（结构体） | — | 不变 |
| `put(&key, &R)` | 静态段 + 面 + 账本 | 1 | 不变 |
| `delete(&key, &R)` | 静态段 + 面 + 账本 | 1 | 不变 |
| `view(&key) -> Option<BTreeMap<String, DynamicValue>>` | 两槽的合并投影 | — | `get_document` + `get_fields` |
| `merge(&key, &map)` | 声明名 → 行 RMW；其余名字 → 槽 1 里对应的那一个名字 | 1 | `put_document`（merge 那一半）+ `put_fields` |
| `replace(&key, &map)` | 由 map 重建行；槽 1 = map 里非声明名的集合 | 1 | ——（比 `put_document` 语义强） |
| `purge(&key) -> bool` | 两槽一起清除 | 1 | `delete_document` + `delete_fields` |

1. **静态段说结构体。** `get` / `put` / `delete` 契约不变。尤其是 `put` 保持"不清旧面、由 `delete(旧行)` 收尾"——`index_test.rs` 有意锁着这条。
2. **整文档说一个名字键 map。** `view` 投影两槽；只有动态段的文档也能 view，因为文档是**两槽这一对**，不是主条目。
3. **`merge` 只动点名的字段。** 声明名是一次行读-改-写，走语义路径（`save_with_prev_into`），所以旧值产生的面**会**被清掉、reduce 账本也会回折补偿。其余名字覆盖槽 1 里的同名项，别的名字不动。map 里没有声明名时完全不写行。
4. **`replace` 把 map 当成整份文档。** 声明名重建行；map 没点名的声明字段取 `from_map` 的默认值（payload 是位置编码，半行编不出来）。槽 1 恰好是 map 里的非声明名，所以 `replace(&空 map)` 就是 `purge`。map 里没有声明名时静态段缺席，原来有行就删掉。
5. **`purge` 清两槽**，一次提交，并回答原来有没有东西。
6. **每次写一个引擎提交。** 行的全部 op 和槽 1 的条目攒进同一个 `MemBatch`，一次落到引擎，崩溃时要么都落要么都不落。
7. **六个旧名直接退役，不留别名**（1.0 之前；消费方骑 git 依赖，同一会话内扫完）。
8. **`Graph` 的边属性面同样去掉按槽切分的词汇**：`get_fields` → `attrs`，`put_fields_into` → `replace_into`。那个函数是整段替换，叫 `merge_into` 会描述一件它没做的事；边的 typed 半边是 payload 而不是 map，叫 `view` 同样超出事实。

## 为什么不

- **新旧并存**。同一份文档两套写法，其中一套名字是错的（`put_document` 行为是 merge）、原子性只有一半。留下的代价是每个后来的读者都要先学会布局才能选。
- **把 `put` 也改成收 map（只留一个写动词）**。typed 路径是快路径：结构体写不分配 map，而 `put(&R)` 正是每个消费方手里已有的东西。为了统一拼写去给它加税，只服务了外部数据这一个入口。
- **为声明字段生成 setter**。一个字段一套接口面（1000 字段 = 1000 个方法），而每个 setter 本来就是整 payload 的读-改-写——和 `merge` 用一个名字做的事完全一样。结构体进、结构体出更小也更快。
- **让 `merge` 继承 `put` 的面契约（把旧条目漏在库里）**。整文档入口手里没有旧行，无法执行那条契约委派给调用方的清理。`DynamicCollection::put` 早就在清；两个入口不该有两套面生命周期。
- **在这里顺手拒绝"名字命中声明字段但值类型不符"**。`from_map` 现在退化成默认值（`_ => #dflt`）。改成指名 panic 是对的，但那是 derive 的改动、影响面独立——单独立项，不夹带在改名的提交里。

## 后果

- 一次语义写一次提交：崩溃不会再出现主条目已落、面与账本没落。`write_commit_test` 锁着 op 数——插入 3、覆写 4、需要删旧面时 +1、每个槽 1 条目 +1。
- 调用方不再选槽。外部 map（HTTP body、解出来的 Parquet 行、宿主语言 dict）用 `merge` / `replace` 写，用 `view` 读。
- `view` 会回答只有动态段的文档；旧的 `get_document` 在主条目缺席时一律返回 `None`，把那段藏了。
- 成本：`view` 两次点读（行 + 段），`merge` 多一次用于行的 RMW。
- `okm-dynamic` 保留自己的 `DynamicCollection` 接口面（它不包 typed 装配点），并在整文档上补齐同样三个写动词；它的 plan 面将为一份文档产出同一张 op 列表，远程 plan 仍然一次 replay。