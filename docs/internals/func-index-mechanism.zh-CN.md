# 函数索引机制：func、IndexFuncValues 与多值展开

本文是函数索引（`func(path)`）的机制与实现细节文档：声明的展开方式、值的编码契约、多值展开（fan-out）怎么发生、读写两侧如何被同一条声明驱动。条目布局、扫描与回表的通用机制见[索引机制](index-mechanism.zh-CN.md)；建模纪律见[建模指南](../MODELING.zh-CN.md)。

## 定位

函数索引回答的问题是：**数据段不是某个字段的直接编码，而是一行的派生值**。典型场景：

- 归一化——`lower(name)` 让 `Apple`/`APPLE`/`apple` 落进同一段（大小写不敏感索引）；
- 倒排——`tokens(text)` 把一行拆成多个 token，每个 token 一条 entry（全文检索、多值字段）；
- 分桶——`hour(created_at)` 把时间轴折进固定区间。

`fields(...)` 是零成本的（直接编码字段字节），函数索引每次写入多付一次函数调用加一次分配——所以它是 fields 表达不了时的升级路径，不是默认形态。

## 声明展开

```rust
#[kv_index(by_token { func(tokens) })]
```

derive（`okm-derive/src/row_encode.rs`）对每个 func 索引生成同款 marker struct `__OkmIndex_{Row}_{name}`，实现 `KvIndex` 时与普通字段索引的差异只有一处：

```text
const FUNC: &'static str = "tokens"    // 普通索引为空串
```

FUNC 目前是**文档性常量**（记录路径名，测试断言它锁定声明）；真正驱动写入的不是这个常量，而是下文的 `entry_pairs` 覆盖——func 索引与普通索引的机制分岔点在写侧，不在常量里。

## 值的编码契约：IndexFuncResult 与 IndexFuncValues

函数返回值经过两层 trait 编码为数据段（`okm/src/index.rs`）：

```text
IndexFuncResult      单个值的字节编码（数据段格式）
  impl: String       → 裸 UTF-8 字节（变长）
  impl: u8..u64      → BE 定宽

IndexFuncValues      一次 func 调用产生的 entry 集合（单值/多值两臂）
  impl: 单值类型     → vec![该值编码]（经典函数索引，一条 entry）
  impl: Vec<V>       → 每个 V 一条（多值 regime，N 条 entry）
```

单值臂与多值臂不 overlap：`Vec<V>` 自身不实现 `IndexFuncResult`。不采用 `IntoIterator` blanket 的原因正是 coherence——它与单值臂必然冲突（`Vec<V>` 也是 IntoIterator）。func 返回 `Vec` 是契约而非偷懒：put 收到返回值后立即逐条写入，惰性迭代器没有收益，一次 `collect` 换 trait 面最小。

编码方式沿用了字段编码的既定约定：`String` 裸 UTF-8（变长、字典序 = 字节序），整数 BE（定宽、数值序 = 字节序）。函数索引因此自动继承数据段的全部约束——至多一个变长段且必须紧贴主键前缀（func 值就是那个唯一的变长段，主键前缀从尾部反推）；func 值之后再拼定宽字段在当前 derive 下不出现（func 与 fields 互斥，一条声明二选一）。

## 写侧：entry_pairs 覆盖与 fan-out

普通字段索引用 `KvIndex::entry_pairs` 的 trait 默认实现（一对 `(entry_key, entry_value)`）。derive 对 func 索引生成覆盖（这是 func 索引唯一的机制分岔点）：

```rust
fn entry_pairs(table_ns, key, row) -> Vec<(Vec<u8>, Vec<u8>)> {
    let fv = tokens(row);                       // 调用用户函数
    let vals = IndexFuncValues::func_values(fv); // 单值 → 1 条；Vec → N 条
    for seg in vals {                            // 每条 entry 共享同一 includes value
        [ns 2B][slot 1B][seg][主键前缀]          // key；value = entry_value(key, row)
    }
}
```

要点：

- **每条 entry 共享同一 includes value**——倒排场景下 value 是整行 payload 的覆盖段，N 个 token 指向同一行的同一份物化。
- `index_entries`（put/delete 的实际入口）对每个声明索引调用 `entry_pairs` 并展平，对 fan-out 无感知：写循环只看见"N 对"，普通索引是 N=1 的特例。
- delete 用同一函数生成待删集合（`Table::delete` 逐条 `del`）——只要 func 是纯函数，写入多少条就删除多少条，**纯度是契约**：func 内读外部状态（时钟、随机数）会让 delete 生成与写入不同的集合，留下悬挂 entry。

## 读侧：探针归一化，无新扫描路径

读侧零新增机制——token 就是数据段，`scan::<I>` 照常工作：

```text
t.scan::<ByTag>(b"rust")
  1. entry_prefix = [ns 2B][slot 1B] + b"rust"
  2. 范围扫 → suffix 尾段切出主键（KEY_LEN 反推）→ 回表
```

**探针归一化是调用方的义务**：查询侧把 probe 值喂给同一条 func 再编码（测试里的 `probe.name.to_lowercase()`），一条声明驱动两侧。derive 无法替你做这件事——probe 在调用方手里，不是 row。这意味着 func 的编码必须与写入时完全一致（同一函数、同一序列化），一致性由"复用同一个函数"保证，这正是把路径写进声明（而非调用点手写编码）的收益所在。

多值扫描的语义与普通最左前缀完全一致：`b"rust"` 命中所有含该 token 的行；做整 token 精确匹配是调用方的扫描边界问题（变长段无终结符，前缀扫天然是"以此为前缀"，精确匹配可以扫完当前 token 区间——下一个 entry 的 token 段变化即止）。

## 扩展点分层：为什么 func 是 fn 而不是 trait

对外扩展面收在三层，各司其职：

```text
声明式（无扩展点）   fields / includes / key      宏直接生成，用户代码不进入编码
业务派生逻辑         func(path) — 一个普通 fn      唯一让用户代码进入条目编码的口子
值编码接缝           IndexFuncResult              新值类型 → 数据段的字节编码（用户自己的类型，orphan rule 无碍）
                     IndexFuncValues              新展开形态 → 单值/N 条 entry
```

扩展的正确分工：新值类型实现 `IndexFuncResult`；新展开形态给 `IndexFuncValues` 加臂；业务派生逻辑写一个 fn。func 不是 trait，理由有四：

1. **能力零增益**——trait 能做的事 fn path 全能做。需要"同一概念 func 的多个实现"，写两个 fn 就是两个实现；索引声明是编译期的，`func(lower_v1)` / `func(lower_v2)` 直接可选。
2. **trait 破坏"一条声明驱动两侧"**——探针归一化现在就是"调用同一个 path"；换 trait 后探针侧要指定同一个 impl，调用方语法更重，机制没变。
3. **最小接缝**——derive 只需要"一个可调用路径"，fn 是最简满足；包一层 trait 是给业务逻辑强加框架仪式（`fn tokens(&Doc) -> Vec<String>` 写完即止，这是现设计的优点）。
4. **声明即注册**——trait 化的真实动机往往是让框架感知用户逻辑（注册、发现），而 `#[kv_index]` 本身就是注册点，函数只是被指到的实现，不需要第二层簿记。

## 与 fields 的分岔总结

```text
             fields(...)              func(path)
数据段来源    payload 字段直接编码      函数返回值编码
写侧 entry    trait 默认（1 对）        entry_pairs 覆盖（1 或 N 对）
编译期约束    多字段组合、变长位置检查   单值；变长约束由值类型继承
探针          调用方编码 probe 值       调用方调同一 path 归一化 probe
成本          零额外                    每次写入一次函数调用 + collect
```

函数索引不引入新的条目布局、扫描路径或运行时簿记——它只是把"数据段从哪来"从字段读取换成函数调用，其余全部复用索引机制的既有设施。
