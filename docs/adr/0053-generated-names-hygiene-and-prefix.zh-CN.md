# ADR-0053：生成的命名——局部变量靠卫生性，item 名靠前缀

Date: 2026-11-06
Status: Accepted

## Context

派生宏往调用方的 crate 里生成代码：一个 `Document` impl、索引标记类型、前缀 builder，以及各个编解码函数的函数体。这些函数体需要自己的名字——一个字节切片、一个游标、一个长度、一个临时值——而调用方的声明也往同一批作用域里放名字：每个声明字段一个局部变量，由 `let <字段> = …` 绑定，再由结构体字面量的简写（`Self { 字段 }`）读回。

过程宏造出来的标识符默认带 `Span::call_site()`——`quote!` 的文档就是这么写的——所以它们解析起来和调用方亲手敲的没有区别。于是名为 `b` 的负载字段和解码器的字节切片参数 `b` 是同一个名字。本决策之前实测到的现象：`struct S { b: u64, … }` 上的 `DocumentEncode` 编译失败，报 `E0599: no method named 'len' found for type 'u64'`，位置指向派生宏——字段的 `let b = …` 遮蔽了函数体后面一直读的那个切片。`offset`、`hot_end` 是同一类失败（类型不匹配）；而名为 `ver` 的字段今天能编译也能读对，只因为遮蔽点之后再没有人读 `ver`——换一次编辑就会静默变错。

`KeyEncode::decode` 里本来也有同一类：它在一个同时有 `b` 参数和 `offset` 游标的作用域里，为每个键字段绑一个局部变量。

## Decision

**两类生成的命名，两条规则。**

**1. 生成函数体内的局部变量带 `Span::mixed_site()`**——就是 `macro_rules!` 的规则：局部变量与 label 在宏的定义处解析，其余一切（类型、宏、路径）仍在调用处解析，所以生成的函数体照样能写出调用方的类型。

- 名字不变：解码器的局部变量仍然叫 `b`、`offset`、`hot_end`、`v`、`len`。可读性和 `cargo expand` 的输出都不退化。
- 名字的身份是 `(symbol, syntax context)`，所以生成的 `b` 和字段的 `b` 无论打印成什么都是两个名字。这是结构性保证：不靠概率、不占命名空间、没有要记的前缀。
- 插值进来的 token——字段 ident、字段类型、声明的默认值——保留自己的 span，因而仍是 call-site。这一半是必须的：`Self { 字段 }` 和 `self.字段` 都还得读得到那个绑定。

必须守住的规则：**一个把输入派生的 ident 绑定成局部变量的生成作用域，必须把它自己的每一个局部变量都按 `mixed_site` 发出来。** 今天符合这条的是三个解码器——`Document::decode_payload`、`KvIndex::decode_covered`、`KeyEncode::decode`。编码侧的命名遍历、map 桥、探针 builder 都不绑定输入派生的局部变量，所以它们保持 call-site。机制落在 `okm-derive/src/hyg.rs`（`mixed()`、`ident()`、`mix()`）；各类型的 `dec` 片段在构造字段 schema 的那一处统一重设 span，而不是在那三十来处声明里逐个包裹。

**2. item 名保持 call-site，并保留 `__Okm` 前缀**——`__OkmIndex_{Row}_{index}`、`__OkmIndexPrefixBuilder_*`、`__OkmPrefixBuilder_*`、`__OkmCovered_*`。这些名字调用方必须能写出来（`scan::<__OkmIndex_Row_by_tag>(&probe)`、签名里的探针类型、covered 结构体的字段），所以它们必须能从声明推出来，而前缀正是让一个生成类型不落在普通名字路上的东西。何况卫生性本身够不到它们：`mixed_site` 只管局部变量、label 和 `$crate`，而能让 item 名也变成定义处名字的 `def_site` 是不稳定的（`proc_macro_def_site`；在 `proc-macro2` 里是 semver-exempt）。

## Rejected：给局部变量加前缀，或者加随机后缀

- **局部变量用 `__okm_b` 这类前缀。** 和任何保留命名空间一样是交换：把碰撞从不可能变成极不可能，同时把一个内部名字搬到调用方看得见的表面上，可以被人意外写中（`__okm_` 在语法上没有任何保护——调用方可以真的声明这个字段）。它还得靠约定在每一个新局部变量处维持，并且白白花掉解码器的可读性。前缀是 item 名该用的工具：那里它是承重的（调用方必须能猜到名字），而且失效形态是重复定义——响亮的编译期错误。
- **每次展开加随机后缀。** 让碰撞概率趋近于零，但它仍然是概率；而且它花掉确定性：同一份源码每次编译产出不同的 token 流，于是 `cargo expand` 的 diff 全是噪音，该模块的增量复用失效，诊断信息开始叫 `b_9f3a2c`。从输入派生后缀（结构体名的 hash）能找回确定性，但它依旧是概率下注，相对固定前缀买不到任何东西——而且它对 item 名不成立，那些名字是调用方必须能写的。
- **整个函数体用 `Span::def_site()`。** 更彻底——item 名也会带卫生性——但它在 `proc_macro` 里不稳定，stable 上够不着。

## Consequences

- 字段、键段、`includes` 条目可以叫任何名字，包括派生宏内部用到的每一个名字。`okm-core/tests/generated_name_hygiene_test.rs` 锁住这条：那里声明的结构体就是按解码器自己的局部变量命名的，所以一旦丢了这条性质，那个文件就编译不过，而不是在用户的结构体上留一个潜伏的坑。
- 这条性质无法用展开快照锁：`proc-macro2` 的 fallback 把 `mixed_site()` 定义成 `call_site()`，且 fallback 的 `Span` 根本没有上下文，差别只存在于 rustc 内部。锁定手段只能是一个必须编译通过的集成测试。
- 诊断仍然指向宏调用处（`mixed_site` 的 span 位置就在调用处），报出来的名字是可读的那套——这正是选择不做名字混淆的理由。
- 往解码器里加一个局部变量，意味着要走 `hyg::mixed()` 发它，或者写在 `quote_spanned!` 的函数体里；`hyg.rs` 写明了这条规则，以及为什么其他作用域豁免。