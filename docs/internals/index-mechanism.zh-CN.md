# 索引机制：条目、扫描与回表

本文是机制与实现细节文档：`#[kv_index]` 声明展开成什么、条目怎么编码、`scan::<I>` 与 `scan_covered` 怎么工作。建模纪律见[建模指南](../MODELING.zh-CN.md)（访问方法强制、覆盖索引克制）；slot/ns 推导见[slot 机制](slot-mechanism.zh-CN.md)。

## 声明展开

`#[derive(RowEncode)]` 对每个 `#[kv_index(name { ... })]` 生成一个**索引类型**（marker struct，生成在展开点）：

```text
__OkmIndex_{Row}_{name}      // 机械拼接，无大小写转换
```

类型实现 `KvIndex` trait（`okm/src/index.rs:122`），携带编译期常量与方法：

```text
type Key / type Row          关联的主键与行类型
const SLOT                   声明序序位（1, 2, …；0 = 主表），进 entry 头第 3 字节
const FIELDS                 索引字段（排序序）
const INCLUDES               覆盖字段（进 value，不参与排序）
const KEY_PREFIX             主键尾段截断子集（空 = 全长）
const FUNC                   函数索引路径（空 = 普通字段索引）
fn encode_named(...)         按字段名序编码 payload 段
```

查询侧把类型当泛型参数：`t.scan::<ByOrg>(&prefix)`；`use __OkmIndex_User_by_org as ByOrg` 只是书写别名，不参与机制。

## 条目编码

```text
entry key   [ table_ns 2B BE ][ slot 1B ][ 索引字段 BE ][ 主键前缀 ]
entry value [ includes 字段 TLV ]                （无 includes = 空）
```

- **索引字段段**：`FIELDS` 按声明序逐个编码，首位 = 分组维度（最左前缀匹配的物理基础）。**至多一个变长字段且必须紧贴主键前缀之前**：定宽段从右往左依次切分（主键 `KEY_LEN` 提供右手锚点），剩下整块是唯一变长段，长度由右侧定宽边界反推、不需存储；两个变长段之间无边界字节，derive 编译期拒绝（MODELING「数据段」节）。变长段不在末位则前缀语义破坏（裸字节无终结符，`"beijing"` 命中 `"beijing2"`）。
- **主键前缀**：默认全长主键编码（`KEY_LEN` 编译期锁死）；`key(...)` 声明截断到命名子集。尾段永远可解——它就是主键编码，布局在声明系统内。
- **函数索引**（`func(path)`）：`path` 的返回值经 `IndexFuncValues` 编码为数据段——单值（`String`/整数）一条 entry（经典函数索引，如归一化）；`Vec<V>` 一行展开为 N 条 entry（多值 regime：tokenize 倒排、多值字段、时间分桶）。查询侧探针调用同一路径，一条声明驱动两侧。展开细节见[函数索引机制](func-index-mechanism.zh-CN.md)。

## scan::<I>：前缀扫描 + 回表

```text
t.scan::<I>(&encoded)
  1. entry_prefix = [ ns 2B ][ slot 1B ] + encoded     编译期选表与序位，运行期拼字节
  2. store.scan_suffix(&p)                             引擎范围扫，返回去前缀的 suffix
  3. 尾段切出主键编码 → PrefixKey<K> { decoded, taken }   宽度已知（key_prefix_width）
  4. 回表：taken == KEY_LEN → get(decoded) + decode_payload
          taken <  KEY_LEN → row = None（截断前缀无法回表）
```

返回 `Vec<(PrefixKey<K>, Option<R>)>`。截断 `key(...)` 的索引只能拿到可信前缀字段，调用方拿 `decoded` 去主表继续前缀扫描。

回表看似逐条随机点查，实际两条流都有序：索引条目按 `[索引字段][主键前缀]` 聚簇，主表按 `[主键]` 聚簇——顺序 I/O 的双指针归并（merge join），量化分析见[建模指南「设计约束对性能的影响」](../MODELING.zh-CN.md)。

## scan_covered：覆盖扫描

`includes(...)` 字段复制进条目 value，`scan_covered::<I>` 免回表：

```text
store.scan_suffix_kv(&p)            连 key 与 value 一起取
  → 尾段仍从 key 切出（宽度同上）
  → value TLV 解出 includes 字段
```

返回 `(PrefixKey<K>, Vec<u8>)`（value 为原始 TLV 字节）。它是高扇出查询的物化视图——每次 put/delete 付永久写成本，纪律见[建模指南「覆盖索引克制」](../MODELING.zh-CN.md)。

## 写路径的同步

`Table::put` 单次写入：主表条目（slot 0）+ 每个声明索引的 `entry_pairs()` 全部条目（普通索引一对；多值函数索引 N 对），同一 store 实例内；`Table::delete` 对应删除全部。没有运行时索引簿记——`index_entries()`（derive 生成）静态展开为每个索引调用 `entry_pairs` 并展平，声明即注册。**因此条目与声明永不失配**：库里存在哪个 ns 段的条目，当且仅当源码里声明了对应索引。

## 两种"一对多"的分野

同表 payload 维度 → `fields(...)`（本文机制）；跨表实体关系 → `EdgeEncode` 双向边（见[建模指南「多对多关系」](../MODELING.zh-CN.md)）。二者机制同构（次级 key 布局指向身份），分界在数据源：索引 = 本行 payload 的派生视图，边 = 一等的关系数据。
