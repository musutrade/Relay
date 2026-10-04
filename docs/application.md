# 应用运行与恢复

## 边界与部署方式

`relay` 是不透明队列库与 CLI。`relay-app` 是独立 workspace package，复用同一 SQLite 状态转换，包含 HTTP/UI、MCP、开发 job 校验和可信 Linux 宿主。Axum/Tokio 仅用于应用 HTTP；libc 用于独立 supervisor 的 Linux 子进程管理。没有动态插件系统或远端 worker 协议。

应用目前针对单个可信本机账户。数据库、配置文件、工作区根和外部 CLI 凭据必须由该账户控制；不要授予不可信用户写权限。HTTP token 是所有已配置仓库的单一操作能力，没有多用户权限划分。服务拒绝非回环监听地址，默认 `127.0.0.1:8787`。需要远程访问时由运维人员提供 TLS 和受控网络入口；程序不会自动配置。

同一个数据库只运行一个 `serve` 实例；并行 MCP 客户端只提交/读取。SQLite 仍会阻止不同进程同时领取，但另一个 server 的活跃 claim 在本实例显示为无法确认的执行。不要把多个 HTTP 实例当作高可用集群。

## 配置与 job

运行 `python3 examples/make-demo-config.py` 可生成所有路径均为绝对路径的有效示例。配置的主要字段：

- `workspace_root`：任务快照根目录，与源仓库不得互相包含
- `repositories`：名字 → 本机仓库目录
- `agents` / `tests` / `draft_pr_adapters`：名字 → `{program,args,env}`。程序使用已存在的绝对路径；参数数组不经过 shell。只有配置者可以选择程序和固定参数
- `timeout_seconds`：整个快照、Agent、测试和 PR 流程的共同期限，1–3600 秒，默认 300
- `output_limit_bytes`：每阶段 stdout/stderr 分别捕获 1–8192 字节，默认 2048；多余内容继续排空并标注截断，不无限累积
- `max_snapshot_bytes` / `max_snapshot_entries`：复制输入快照时的字节数和条目限制，默认 50 MiB / 20,000；复制排除 `.git`、`target` 和 `node_modules`，拒绝符号链接和特殊文件
- `max_retained_workspaces`：默认最多保留 100 个工作区；达到上限后拒绝新执行，需可信操作员先检查和清理。运行期每 250ms 尽力检查相同的字节/条目预算，超限会停止当前命令；快写入可能暂时超过限制，这不是内核文件系统配额

开发 job 示例：

```json
{
  "repository": "demo",
  "requirements": "创建演示结果并运行配置的测试",
  "agent": "fake",
  "test": "demo",
  "publish": false
}
```

`requirements` 必须是 1–32768 UTF-8 字节。repository / agent / test 只接受配置名字；未知字段、未知配置、空需求或超限输入被拒绝。`test` 可省略或为 null。提交 HTTP 请求的外层是 `{ "key": "client-chosen-stable-key", "job": ... }`。key 由客户端保存，提交结果未知时复用；成功后下一项需求换新 key。

参数中只有独立的完整 token 被替换：`{requirements}`、`{requirements_file}`、`{workspace}`、`{repository}`、`{task_id}`、`{generation}`。例如 `"--prompt={requirements}"` 不会展开；请使用两个参数 `"--prompt", "{requirements}"`。需求也通过 stdin、`RELAY_REQUIREMENTS` 和 `RELAY_REQUIREMENTS_FILE` 提供。各阶段工作目录为任务的 `repository/` 快照，其他环境包含 `RELAY_WORKSPACE`、`RELAY_REPOSITORY`、`RELAY_TASK_ID`、`RELAY_GENERATION`。HTTP 的 `RELAY_TOKEN` 不传给任务进程。

外部 CLI 可以使用其已有的账户与模型配置。Relay 不安装模型 CLI、不创建 OAuth/API key、不猜测各版本的付费模型参数。配置者需确认 CLI 在非交互模式下可运行，并负责授予其访问范围和费用预算。即使参数不经过 shell，配置的 Agent 依然可以执行代码；这不是防恶意代码沙箱。

## 可选 draft PR 适配器

不提交 `publish=true` 时绝不调用 PR 阶段。启用时必须同时指定配置中存在的 `draft_pr_adapter`，并且 Agent 与所有已配置测试阶段都成功。网页默认只运行开发/测试；PR 阶段通过 HTTP 或 MCP 明确选择。

`examples/github-draft-pr.py` 提供真实 Git/gh 命令适配器，默认仅输出 dry-run 计划。配置者需要显式设置 `RELAY_GITHUB_REPOSITORY=owner/repo`，可选 `RELAY_GITHUB_BASE=main`；Git/gh 路径默认 `/usr/bin/git`、`/usr/bin/gh`，可用 `RELAY_GIT_PROGRAM` / `RELAY_GH_PROGRAM` 指向已安装程序。只有可信配置设置 `RELAY_GITHUB_EXECUTE=1` 才实际发布，要求 Git/gh 已登录且 Git 已配置提交身份。

真实模式会在快照初始化 Git，拉取目标 base、保留快照文件并建立基线、提交、推送 `relay/task-<id>-g<generation>`，最后 `gh pr create --draft`。它不会 merge、deploy 或 force-push。注意：目标 base 应对应复制的源仓库；源快照与远端基线不一致时会产生额外差异。首次使用先看 dry-run，并使用测试仓库核对差异。实际发布也会调用 git 网络操作，必须事先授权该目标仓库。

如工作区已有 `.git` 或任何命令失败，适配器停止，不自动清理、覆盖或重复推送。网络失败不能证明 PR/分支没有创建，必须人工检查远端；Relay 不承诺跨 GitHub 副作用恰好一次。`examples/fake-draft-pr.py` 和标准库 mock 测试不产生任何真实远端副作用。

## HTTP 与 MCP

HTTP 所有 `/api` 路由校验 bearer token（32–256 非空白 ASCII 字节）。HTML 入口无需 token，但无 token 不能读取任务或配置。页面内存保存 token，没有 URL/token 持久化；响应 `no-store`，无第三方脚本和 CORS 开放。用户可查看任务、提交、请求取消；接口没有运行任意命令、自动恢复或删除工作区入口。

MCP 采用 stdio newline JSON-RPC，支持 `initialize`（协议 2024-11-05）、`ping`、`tools/list`、`tools/call`。工具为 `relay_submit`、`relay_get`、`relay_list`、`relay_config`，只作需求入口和读取结果。每条消息上限 128 KiB；没有 ID 的通知不返回响应，也不会提交任务。MCP 是可信 OS 本机进程接口，不使用 HTTP token。MCP 本身不启动 worker，因此需要同时运行 `serve`。

MCP 配置示例（把路径替换成实际绝对路径）：

```json
{"command":"/absolute/path/relay-app","args":["mcp","/absolute/path/config.json","/absolute/path/relay.db"]}
```

## 状态、取消和未知结果

核心仍只有 queued / claimed / finished。`finished` 表示结果已持久化，不等于业务成功。应用结果的 `outcome` 为 success / failure / cancelled / timed_out / unknown，包含各阶段的退出码、截断标记、输出、时间和工作区路径。

取消请求按逻辑任务持久化，人工重排不会清除该请求。取消排队项将请求持久化；该项轮到执行时会领取并立即记录 cancelled，不运行 Agent。取消本进程正在执行的任务会通知 supervisor，停止并回收子进程树；HTTP `requested=true` 仅说明请求收到，不说明已停止。取消接口不会谎称重启前或另一个 server 的进程已终止，而是返回 409。

每个阶段由独立 Linux subreaper supervisor 运行。它排空有界输出、施加共同 deadline，并在取消/超时/父进程断开后终止和回收其可观察的子进程。未知 supervisor 状态或无法确认的清理不会释放 claim；`/api/status` 返回 `recovery_required=true` 和匹配 generation 的诊断（如有）。信任范围不涵盖恶意逃逸、脱离宿主控制的外部服务或已发送到远端的副作用。

重启时不会自动领取旧 claimed 任务，不设置 lease，不按等待时间自动重排。恢复顺序：

1. 停止本服务领取新工作，读取 `relay <db> active` 的精确 id/generation/owner
2. 结合工作区内 supervisor 记录和实际 OS 进程核对旧执行及其后代确实已终止；PID 可能复用，不能仅据 PID 杀死无关进程
3. 检查 GitHub、外部服务和文件结果，决定是否可以重做；保留原工作区和诊断
4. 确认后，可信本机操作员运行 `relay <db> confirm-stopped-and-requeue <id> <generation> <owner>`；新 generation 用新工作区，旧 generation 不可写回

若旧任务实际上已成功而只有落库失败，也可用同一 claim 的 `finish` 写回已核实结果。恢复操作有意不暴露到网页。不要通过手工删除数据库或工作区消除未知状态。

## 验证

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
python3 -m unittest discover -s app/tests -p '*_test.py'
node app/tests/ui_logic_test.cjs
```

测试覆盖原内核并发/fencing/restart，以及 API 认证、幂等、输入限制、跨进程取消竞争、过期 generation、MCP 协议校验、模拟 Agent/测试/PR、进程树取消与超时。真实付费模型、真实 GitHub 发布、远程网络部署和恶意代码隔离不属于离线测试结论。


UI 的依赖免费 Node 状态测试覆盖重复提交、保留幂等 key 重试、认证过期、退出取消请求、旧响应/新选择竞争、网络恢复、活跃任务补入列表和未知诊断。可选真实浏览器脚本为 `python3 app/tests/ui_smoke.py`，使用 `app/tests/browser-requirements.txt` 固定依赖和官方 Playwright Chromium。当前 dot cloud 环境阻止浏览器启动/访问 localhost；GitHub CI 提供独立 browser job 执行桌面/手机 viewport 与交互断言并保存仅含假数据的截图。是否通过请以该提交的 CI 结果为准，不能将状态测试替代真实浏览器验收。
