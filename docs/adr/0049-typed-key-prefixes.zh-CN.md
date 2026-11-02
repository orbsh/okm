# ADR-0049: 有类型的主键前缀是一个 builder，前缀扫描是惰性的

状态：Accepted（2026-11-02）。

## 背景

合成主键自己就带着一层分组：`[org][user][device]` 用一次前缀扫描回答"这个用户的全部设备"，MODELING 写明了理由——key 本身就是查询。但这道门此前只在下一层存在：手工拼出 `[partition?][ns][slot 0]`，再追加前导字段的大端字节，然后调 `store().scan_suffix`。这是层次穿透，且失败是静默的——漏掉 partition 转义段，扫出来是空，不是报错。

索引侧有门但很窄：`scan` 返回 `Vec`，LIMIT 无法让遍历提前停下，要 newest-first 只能先把整组物化再反转。`scan_range_iter` 是惰性的，但它讲的是显式区间——前缀得由调用方自己翻译成 `[begin, prefix_end)`。

## 决策

1. **探针是类型，不是字节切片。** `PrefixProbe<K>` 携带编码后的前导字段，只有 `Collection<S, K, R>` 接受它，所以探针不会花在错的表、错的 key 上。
2. **探针由 derive 发射的类型状态 builder 构造**：`DeviceKey::prefix().org(7).user(11).finish()`。每个深度一个零尺寸状态类型、每个状态一个字段方法，声明顺序因此是编译期规则。任意深度都能调 `finish()`——空探针不约束任何字段，流式走完整张表。
3. **`Collection::scan_keys(probe)` 返回惰性 `DoubleEndedIterator<Item = Result<K>>`**，扫槽 0 的行；`scan_document_keys(probe)` 收同一个探针，用于两槽的文档模式（槽 0 ∪ 槽 1，保持急切：归并需要两侧同时在手）。全表扫描就是**空探针**——`PrefixProbe::empty()`——不是另一个门，所以原先零参数的枚举形态不是并存，而是被取代。行模式下惰性就是接口：`.take(n)` 是 LIMIT，只为真正执行的读取付费；`.skip(m).take(n)` 是翻页；`.rev()` 对同一窗口反向走。
4. **索引侧拿到同一形状，也拿到同一套类型。** `scan_prefix_iter::<I>` 是 `scan` 的流式对偶，两者（以及 `scan_exact`）都收探针：derive 发射的 `IndexProbe<I>`（`ByOrg::prefix().org_id(7).finish()`，按声明的 `fields(...)` 顺序、每个字段一个方法），或者给"探针由数据拼出"场合的裸编码字节。索引的标记类型是那道闸——一个 `org` 探针花不到 `by_label` 上。`ProbeBytes<I>` 是让两者共用一道小门而不加第二个泛型参数的 trait（加第二个参数会打断每一处 `scan::<I>` 的 turbofish）。前缀与区间迭代器共用私有的 `entry_iter`，边界以所有权传入——edition 2024 的不透明返回类型会捕获作用域内所有生命周期，借用来的边界字节会被一并捕获。`func(...)` 索引不发射 builder：它的键字节是函数的输出，不是字段。
5. **探针之下长度与声明不符时报错**（`Error::Corrupt`），绝不跳过——与 `scan_keys` 面对过短主条目时的立场一致。
6. **否决：每个前缀派生一个结构体**（`X { a, b, c }` → `X_ab`、`X_a`）。它把 key 布局变成公开 API——每个前缀名都是调用点名；在两者之间插入字段会让既有前缀静默绑定到另一段字节：同名、不同字节、编译器一声不吭。每个类型还有一半天生是死代码：`Collection` 由 `K` 定参，扫描返回 `K` / `PrefixKey<K>`，永远不是那个窄类型。
7. **否决：名字列表**（`prefix_of(&[("a", 7)])`）。它在运行期校验声明在编译期已经知道的事，未知名或乱序名从调用点的编译错误退化成 panic。

## 后果

- builder 与其状态类型发射在调用方 crate 里（对外部泛型类型做 inherent `impl` 不合法），标 `#[doc(hidden)]`，除 `prefix()` 的返回类型外不出现在任何用户书写的签名里。
- `PrefixProbe::new` 仍对宿主注入的引擎开放，文档标注为不校验：与布局不符的字节会静默扫错窗口。
- 这道门只回答前缀读。不是主键前导段的维度，是 key 布局问题或访问方法问题；这类段的镜像 workaround 要等 `#[ok_index]` 能命名 key 段才退休，本 ADR 不做这件事。