# 应用运行与恢复

完整真实 CLI 配置、用户目录安装、信号停止、备份与升级步骤见 [Linux 操作手册](operator-guide.md)。

## 边界与部署方式

`relay` 是不透明队列库与 CLI。`relay-app` 是独立 workspace package，复用同一 SQLite 状态转换，包含 HTTP/UI、MCP、开发 job 校验、有界审查修复工作流和可信 Linux 宿主。Axum/Tokio 仅用于应用 HTTP；libc 用于独立 supervisor 的 Linux 子进程管理。没有动态插件系统或远端 worker 协议。

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

`requirements` 必须是 1–32768 UTF-8 字节。repository / agent / test / workflow 只接受配置名字；未知字段、未知配置、空需求或超限输入被拒绝。`test` 可省略或为 null。提交 HTTP 请求的外层是 `{ "key": "client-chosen-stable-key", "job": ... }`。key 由客户端保存，提交结果未知时复用；成功后下一项需求换新 key。

参数中只有独立的完整 token 被替换：`{requirements}`、`{requirements_file}`、`{workspace}`、`{repository}`、`{task_id}`、`{generation}`。例如 `"--prompt={requirements}"` 不会展开；请使用两个参数 `"--prompt", "{requirements}"`。需求也通过 stdin、`RELAY_REQUIREMENTS` 和 `RELAY_REQUIREMENTS_FILE` 提供。各阶段工作目录为任务的 `repository/` 快照，其他环境包含 `RELAY_WORKSPACE`、`RELAY_REPOSITORY`、`RELAY_TASK_ID`、`RELAY_GENERATION`。HTTP 的 `RELAY_TOKEN` 不传给任务进程。

外部 CLI 可以使用其已有的账户与模型配置。Relay 不安装模型 CLI、不创建 OAuth/API key、不猜测各版本的付费模型参数。配置者需确认 CLI 在非交互模式下可运行，并负责授予其访问范围和费用预算。即使参数不经过 shell，配置的 Agent 依然可以执行代码；这不是防恶意代码沙箱。

## 有界开发、测试、审查与修复

配置 `workflows` 可启用精确候选提交工作流；未选择工作流的普通任务仍复制快照，并使用 `/usr/bin/git` 初始化新的私有仓库边界。它不复制源仓库的历史、remote、配置或 hooks，也不自动提交快照；`git rev-parse --show-toplevel` 指向任务的 `repository/`，`git status` 只列出该快照内的文件。初始化失败时不启动 Agent。各命令的 Git 向上发现范围限制在工作目录的父目录，私有 `.git` 被移走后也不会识别到宿主仓库。工作流配置只由可信部署者修改，例如：

```json
{
  "workflows": {
    "reviewed": {
      "repository":"project",
      "developer":"codex",
      "reviewer":"claude-reviewer",
      "test":"check",
      "git_program":"/usr/bin/git",
      "max_repairs":1,
      "draft_pr_adapter":"github",
      "github_repository":"owner/repository",
      "base_branch":"main"
    }
  }
}
```

这是完整 host 配置的片段；所有名字必须引用现有允许项。`test` 必须配置，`max_repairs` 默认 0、最大 3，表示初次开发之后最多自动修复几次。若不需要发布，同时省略 `draft_pr_adapter` 与 `github_repository`。当前 reviewer 必须是支持受限只读工具的 Claude 原生 profile；通用命令和 Codex reviewer 在执行开发前被拒绝。版本/能力不足也直接失败，不降级权限。

```json
{"repository":"project","requirements":"实现需求并补测试","agent":"codex","workflow":"reviewed","publish":false}
```

工作流固定 repository、developer、reviewer 和 test；job 的 `agent` 必须等于该 developer，若给出 `test` 则必须匹配配置。网页可选工作流并自动锁定这些字段；网页仍不请求 GitHub 发布。HTTP/MCP 可显式选择 `publish=true` 和匹配的 `draft_pr_adapter`，不能用 job 改模型、命令、修复次数或目标仓库。

执行顺序：

1. 检查 reviewer 能力、干净的本地 Git 源仓库，并固定源 HEAD 为 `base_sha`。工作区通过本机 Git 浅导入该精确提交，不复制源 `.git` 配置或未跟踪文件；不访问远端。拒绝子模块、符号链接和特殊文件
2. developer 修改独立工作区，宿主创建 `candidate_sha`。禁止 developer 自行改变 HEAD；宿主负责提交
3. 对已提交候选运行配置测试，随后验证 HEAD、索引和工作树仍对应同一候选。测试产生的被忽略构建输出可保留，交付内容发生变化则失败
4. reviewer 使用受限只读工具检查完整有界 diff 与候选文件，必须返回包含精确 `candidate_sha` 的 JSON verdict、摘要和 findings。错误 SHA、缺字段、截断回答、权限拒绝、非法终态或修改工作区都不通过
5. 只有普通测试非零退出，或有效 `changes_requested`，才能在剩余预算内触发修复。每次修复生成新候选，重新测试与审查；旧审查结论失效。所有轮次共享总期限，最多保存 4 轮有界证据

工作流 prompt 在保留原始需求上加入宿主指令，使用单独的有界内部输入预算；用户需求仍限 32 KiB。完整 diff 最大 256 KiB，跟踪文件清单最大 64 KiB；超限失败而不让 reviewer 审查截断版本。完整性检查比较实际文件字节和 owner 可执行位与 Git blob，不通过 clean filter 判断；使 checkout 字节不同于提交的 CRLF/encoding/filter 转换暂不支持。拒绝新添加的 gitlink/嵌套仓库，以及私有 Git 元数据的 commondir/alternates 重定向。工作区运行预算涵盖 Git 元数据与证据，仍是尽力检查而非文件系统硬配额。结果增加 `workflow`，含 base/candidate/reviewed SHA、每轮摘要及外部结果核对标志，不增加内核状态，也不提供中途恢复的模型会话。

## 可选精确 SHA draft PR 适配器

`examples/github-draft-pr.py` 只接受已经测试并审查通过的 Git 工作流候选。旧通用演示可继续用 `examples/fake-draft-pr.py`，但真实 GitHub 示例不再在发布阶段初始化 Git、覆盖基线或重做提交。

适配器默认仅输出 dry-run 计划，不启动 Git/gh。可信配置设置 `RELAY_GITHUB_EXECUTE=1` 才实际发布；Git/gh 必须已安装和登录，Relay 不处理凭据。Git 路径由工作流固定；gh 默认 `/usr/bin/gh`，可通过适配器配置的 `RELAY_GH_PROGRAM` 设置。工作流覆盖并传入目标 `RELAY_GITHUB_REPOSITORY` / `RELAY_GITHUB_BASE`、`RELAY_BASE_SHA` / `RELAY_CANDIDATE_SHA` / `RELAY_REVIEWED_SHA` 及测试/审查结果；job 不能覆盖它们。

实际发布前再次检查本地 HEAD、索引、工作树，以及目标远端 base 当前指向已固定的 base SHA；远端发生变化时停止。拒绝 Git URL 重写和命名 remote 映射，保留已有凭据 helper 并固定 gh 的 github.com 主机；禁用 Git HTTP 重定向和自动附带 tag 推送。该检查不是远端锁，之后仍可能有其他人推进 base。推送 refspec 使用精确 candidate SHA，创建 `relay/task-<id>-g<generation>`，并且仅 `gh pr create --draft`。PR 正文记录 base/candidate SHA 和测试/审查结论。适配器不 force-push、merge 或 deploy。

一旦推送或建 PR 已尝试，网络错误、异常退出或无法验证结果都可能已经产生远端副作用。结果标记 `reconciliation_required=true`，不自动重试、不重新开发或创建第二个 PR。确认进程树结束时可保存失败结果；只有进程生命周期无法确认才沿用 claimed/unknown 恢复规则。操作员必须核对目标分支和 PR 后决定下一步。跨 GitHub 副作用不承诺恰好一次。

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

## 原生 Codex / Claude CLI profile

旧 `agents` 命令 profile 保持兼容。`native_agents` 提供封闭的 `codex_cli` / `claude_cli` 协议适配；两类 profile 名称不能重复。job 的 `agent` 仍只引用可信配置中的名字，不接受用户指定程序、参数、环境变量或模型 ID。

```json
{
  "native_agents": {
    "codex": {"provider":"codex_cli", "program":"/absolute/path/codex"},
    "claude": {"provider":"claude_cli", "program":"/absolute/path/claude", "max_turns":8, "max_budget_usd":2.0}
  }
}
```

这是需要加入完整 host 配置的片段。程序必须已安装；可选 `model` 来自部署者自己的账户配置，省略时由 CLI 选择。`effort`、Claude `max_turns` / `max_budget_usd` 也只由可信配置指定。不要把展示名猜成供应商模型 ID。`env` 仅供可信部署者配置已有 CLI 所需环境；Relay 不保存或代办登录、密钥与付款。

```sh
cargo run -p relay-app -- doctor /absolute/path/config.json
```

`doctor` 只调用已配置 CLI 的版本与帮助入口，输出 JSON 能力诊断；不发出模型请求。退出码 0 表示诊断完成，不代表所有 profile 兼容；自动化须检查 JSON 中所需 profile 的 `compatible`，审查能力另看 `probe.read_only_supported`。Claude 的[官方 CLI 文档](https://code.claude.com/docs/en/cli-reference)说明帮助输出不包含所有参数：若配置了但帮助中隐藏 `--max-turns`，Relay 先验证版本和其他必要参数，再用带 `--help` 的缺参/有效值探测确认解析器确实支持该选项；单独 `--help` 成功或普通非零退出均不足以通过。额外探测共享原有 10 秒期限和输出上限；格式不明仍拒绝，不删除 turn/budget 限制。`compatible` 仅表示本地接口满足调用要求，认证与模型访问始终标记 `unknown`。Claude 无人值守运行要求 v2.1.259+ 及相应参数。任务执行前再次进行受 supervisor 管理的检查，所有检查共享任务期限。缺少功能或检查失败时停止，不退回不安全的权限模式。

只读能力单独探测：当前 Claude 需要 restricted 模式、仅 Read/Glob/Grep 工具、禁用 MCP/自定义命令的完整能力。Codex 开发调用可用，但只读审查返回 `review_profile_unsupported`，因为其 read-only sandbox 不等于禁用项目 MCP/hooks。机器上的托管设置仍属于可信部署边界，不承诺对恶意 CLI 或托管 hooks 隔离。

原生调用使用 stdin 传需求、JSONL 输出以及显式权限参数。供应商事件在排空 stdout 时增量解析，独立于用户可见的截断日志；限制单事件、摘要、标识符与 usage 的保留大小。成功需要进程正常退出以及有效成功终态。Codex 的非致命 error item 和失败的工具 item 可由 Agent 后续恢复，不单独视为协议损坏；以最后一条已完成 agent_message 为摘要，仍须有成功 turn.completed 终态。缺失或非法终态、turn.failed、顶层供应商 error、权限拒绝、预算耗尽、非零退出都不能成为成功。格式错误保持失败，但继续有界解析后续行以保留摘要和 usage；超过 64 KiB 的行丢弃至下一换行后恢复解析，后续成功事件不会消除之前的致命错误。结果的 provider 信息保留实际报告的模型与会话标识；未报告的字段不猜测，也不表示 Relay 实现了可恢复会话。

`GET /api/config` 与 MCP 配置结果仅公开 profile 名称、供应商、配置模型/effort 和未知认证状态，不公开程序路径、完整命令或环境。原生适配是 CLI 子进程集成，不是 SDK 或托管模型服务；现阶段离线验收使用假 CLI，真实账户、模型费用与供应商端行为仍需在明确授权的环境单独验收。

接口依据：[Codex 非交互模式](https://learn.chatgpt.com/docs/non-interactive-mode)、[Claude 非交互模式](https://code.claude.com/docs/en/headless)、[Claude CLI 参数](https://code.claude.com/docs/en/cli-reference)。CLI 接口会演进，安装版本与能力探测结果优先。
