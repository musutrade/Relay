# Relay

Relay 是一个小型、本地、持久化的任务交接内核：用 Rust 库和 CLI，把不透明任务可靠地交给外部执行器。当前基础是 SQLite 队列、提交幂等、单任务领取、所有权校验和有界结果。

它不运行模型，不实现开发工作流，也不启动网络服务。外部 Agent 整理需求，适配器提交任务，开发执行器决定如何完成工作；可信宿主负责工作区和进程生命周期。

## 快速开始

需要 Rust 工具链和 C 编译器；SQLite 由 `rusqlite` 的 `bundled` 功能构建，无需另装 SQLite 服务。在仓库根目录运行：

```sh
cargo test
workdir=$(mktemp -d)
db="$workdir/relay.db"
printf '%s' 'Inspect the repository and return a short plan.' > "$workdir/payload.txt"
cargo run --quiet -- "$db" submit example-1 "$workdir/payload.txt"
cargo run --quiet -- "$db" claim worker-1
```

上面的全新数据库中，第一个任务的 ID 和第一次领取的 generation 都是 `1`。执行工作后，用同一次领取的 ID、generation、owner 写回结果：

```sh
printf '%s' 'Plan prepared.' > "$workdir/result.txt"
cargo run --quiet -- "$db" finish 1 1 worker-1 "$workdir/result.txt"
cargo run --quiet -- "$db" get 1
```

已有数据库请使用实际 `claim` 返回的 ID 和 generation，不要硬编码示例值。任务 JSON 包含 `id`、`key`、`payload`、`state`、`generation`、`owner`、`result`。

CLI 成功响应以 JSON 写到 stdout；错误以 `{"error":"..."}` 写到 stderr 并返回非零退出状态。任务 payload 和 result 是 UTF-8 文本，内核不解释其内容，也不会把它们作为命令执行。

## 命令

```text
relay <db-path> submit <key> <payload-file>
relay <db-path> claim <owner>
relay <db-path> finish <task-id> <generation> <owner> <result-file>
relay <db-path> get <task-id>
relay <db-path> active
relay <db-path> confirm-stopped-and-requeue <task-id> <generation> <owner>
```

- 同一 key 与相同 payload 返回原任务；同一 key 配不同 payload 被拒绝
- 同一个数据库最多有一个 `claimed` 任务；剩余任务保持 `queued`
- `claim` 返回任务或 `null`；`null` 可能表示没有排队任务，也可能表示已有任务被领取。用 `active` 查看当前领取或得到 `null`
- payload 最大 64 KiB，result 最大 16 KiB，key 和 owner 为 1–128 字节；按 UTF-8 字节数计量
- `finish` 必须匹配当前任务的 generation 和 owner，完成后进入 `finished`
- 相同 claim 和相同 result 的重复 `finish` 返回原结果；完成后不能改写为不同结果
- 领取不会自动过期。崩溃后仍保持 `claimed`，不会凭时间流逝重试

只有可信宿主确认旧执行进程及其子进程已经停止，才可调用 `confirm-stopped-and-requeue`。命令本身不能证明进程已停止，也不会停止进程或清理工作区。重新领取会获得新的 generation；旧持有者不能再写回结果，但已经发生的外部副作用无法撤销。

## 开发与边界

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

先运行与修改相关的快速测试，必要时再扩大验证范围。不要为小改动引入全宿主扫描或重复的昂贵验证。

数据库路径及其目录必须由可信本地 OS 账户控制。owner 是一致性标识，不是身份认证凭据；能写数据库的调用方处于同一信任域。当前没有网络监听、远程认证、分布式调度、动态插件或 UI。

详见 [架构](docs/architecture.md)、[设计决策](docs/adr/0001-local-durable-core.md) 和 [下一步](docs/next-steps.md)。
