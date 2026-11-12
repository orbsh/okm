# ADR-0061：湖是一个键空间，不是一个目录

日期：2026-11-11
状态：已裁决并落地 2026-11-11（库这一侧）。二进制里的动词仍然吃路径；§4 说明这剩下的一半为什么先留着。§4 的缺口已于 2026-11-12 补上——见 §7。

## 1. 背景

湖的每个动词都是照着 `Path` 写的：drain 建出 `data/`、把 Parquet 写进去再改名到位置；Delta
日志是对 `_delta_log` 的 `read_dir`；vacuum 与 status 对文件做 stat。当消费者是一个人在拷出来的
目录上跑 CLI 时，这是对的。

第一个真正的生产者是一个自己持有状态、又想让仓库能读到那份状态的服务（k10r ADR-0014）——
而"某个地方"指的是桶。给每个动词再教一种形状，等于同一个协议两份实现，而这正是本 crate 存在的
意义所在。

## 2. 决策：一个 trait、两种存储、键而不是路径

**`store::LakeStore`** 就是一座湖的全部存储面：`get`、`put`、`put_new`、`list`、`head`、
`delete`、`describe`。键相对湖根、用 `/`：`<table>/data/<file>.parquet`、
`<table>/_delta_log/<version>.json`、`watermark-<consumer>-<table>.txt`。

两种实现：

- **`LocalFs`**——一个目录。`put` 写旁路文件再改名；`put_new` 用 `create_new` 打开，认领的原子性
  和当初的改名同一个来源。
- **`S3`**——一个桶，经 `object_store`，在**同步**外观后面挂一个自有的单线程 runtime。这些动词都是
  批处理步骤；一座逼调用方自带 runtime 的湖，等于把存储层变成所有人的问题。`put_new` 用
  `PutMode::Create`（条件 PUT），而用 412 表达同一件事的服务器也按"不是我的"处理：调用方要问的
  只有"这个版本是不是我认领的"。

这个 trait 没有目录、没有改名，因为对象存储两者都没有。它有的是协议真正用到的东西：**对象写一次
就不再改**，版本靠一次"键已存在则失败"的写来认领。两种存储把这同一性质摆给 Delta 写入器看，所以
`write_actions` 只有一个函数。

## 3. 跟着动的东西

- **`delta` 说 `(store, table)`。** `read_log` 列日志前缀；`file_summary` 读字节而不是文件；
  `Addition.path` 是键，`Addition.size` 由写入方给出（存储没有 `stat`）。路径形状的动词保留名字，
  加 `_at` 后缀——`commit_files_at`、`files_in_order_at`——三行：用父目录建一个 `LocalFs`，调
  store 形态。一份实现，两张脸。
- **`publish::replace_table`**——生产者的入口：放好 Parquet 文件，提交一个"替换上一版本全部文件"的
  版本，然后删掉被替换的对象。自己持有状态的服务每个间隔每张表发布一个文件，永远用不到变更日志那条路。
- **`snapshot::parquet_io`** 补了 `batch_bytes`、`export_parquet_with_variant_bytes`、
  `export_reduces_bytes`：桶要的是字节，拿着 store 的调用方不该为了拿到字节而绕一个临时目录。

## 4. 没跟着动的

二进制仍然吃路径：`okm-lake --root <log> --out <dir>`、`condense`、`vacuum`、`status`。把它们改成
url 现在是机械活（每个动词的文件操作处正是 `delta` 里已经替换过的那些），但它会改掉 CLI 的参数和
它的测试，而暂时没有消费者需要：从变更日志 drain 是在日志所在的地方跑的，生产者走
`publish::replace_table`。这个缺口写明而不藏起来——今天 `--out` 不接受 `s3://`。

## 5. 验证

`okm-lake/tests/s3_test.rs` 把整条发布路径跑在真桶上，没有配置时自报跳过（`OKM_S3_URL`、
`OKM_S3_ENDPOINT`、`OKM_S3_ACCESS_KEY`、`OKM_S3_SECRET`）。它对着本机 rustfs 跑过：两次发布得到
版本 0 与 1，被替换的对象离开存储，日志只列一个活跃文件，回读的字节带 Parquet magic，对已存在版本
的 create-only 写回答"不是我的"。本地存储的 101 个测试原样通过。

## 6. 后果

- `object_store`、`tokio`、`futures` 进入本 crate 的依赖。这是不手写 SigV4 就说话 S3 的代价，而且
  它们待在 `store` 后面，trait 之上没有人看见。
- 身处 async runtime 的调用方要从 `spawn_blocking` 调这些动词：这个外观自带 runtime，套在另一个
  runtime 里不是 tokio 允许的事。
- `publish` 立即删掉被替换的文件是刻意的：发布者镜像的是活着的状态。想要历史的生产者保留版本、按
  自己的节奏 vacuum。
## 7. 补记（2026-11-12）：二进制也吃 url

§4 的缺口已经补上。`--out`（也写作 `--store` / `--lake`）解析成 `store::LakeUrl`——每种形式一个
分支，不接触任何桶——drain、condense、vacuum、status 全部经由 `LakeStore` 读写。`delta` 早先做到
的事，现在整个 crate 都做到了：按路径取参的动词成为适配层（`*_at`），目录只是两个存储中的一种。
drain 的位标、清单和 Parquet 都过存储，所以桶对每个动词都是湖，而不只是对 `publish` 是。验证：
`okm-lake/tests/cli_test.rs` 在本机湖上把二进制跑通；`cli.rs` 不联网断言每种地址形式。
