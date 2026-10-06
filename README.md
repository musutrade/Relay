# Relay

Relay 将整理好的开发需求交给外部命令行 Agent，在独立工作区中执行、测试并保存有界结果。Rust / SQLite 内核只负责持久队列、幂等提交与所有权栅栏；HTTP、MCP、开发工作流和进程管理都在独立的 `relay-app` 中。

## 先跑完整的无凭据演示

需要 Linux、Git（`/usr/bin/git`，2.28 或更新版本）、Rust（版本固定在 `rust-toolchain.toml`）、C 编译器和 Python 3。演示不调用付费模型、不推送 Git、不创建真实 PR。

```sh
cargo test --workspace
umask 077
mkdir -p .relay
python3 examples/make-demo-config.py > .relay/config.json
# 用你自己选择的 32–256 字节非空白 ASCII 字符串设置本地访问口令
read -rs -p 'Relay token: ' RELAY_TOKEN; echo
export RELAY_TOKEN
cargo run -p relay-app -- serve .relay/config.json .relay/relay.db
```

打开 http://127.0.0.1:8787，输入同一口令，选择 `demo` 仓库、`fake` Agent、`demo` 测试，提交需求。任务会经历排队 → 执行 → 完成；结果包含每阶段输出和工作区路径。`relay-result.txt` 只写入该任务的独立快照，源仓库不变。页面适配手机尺寸，支持重试提交、刷新状态、查看结果和请求取消。口令只保存在页面内存中，刷新后需重输。

默认只监听回环地址。手机远程访问需要你自行配置可信的 TLS 隧道或反向代理；本项目不会自动开放端口或部署服务。不要把无 TLS 的 bearer token 暴露到公网。

## 三个入口，同一个队列

- **网页 / HTTP**：`relay-app serve <config.json> <db-path> [127.0.0.1:8787]`；同一进程运行可信宿主 worker
- **MCP stdio**：`relay-app mcp <config.json> <db-path>`；提供提交、查询、列表和配置工具，另起上述服务负责执行。MCP 是可信本机进程接口，stdout 只输出协议消息
- **内核 CLI**：`cargo run -p relay -- <db-path> <command>`；保留不透明任务的 submit / claim / finish / get / active / confirm-stopped-and-requeue，可用于本机诊断和受控恢复

默认 HTTP `/api/*` 要求 `Authorization: Bearer …`；可选 [单用户密码与浏览器会话](docs/password-login.md)，支持刷新后保持登录、退出和本机密码轮换。迁移时先保留原 token 的 hybrid 模式。主要接口：

```text
GET  /api/config
GET  /api/capabilities            # 已配置 native profile 的缓存能力目录
POST /api/capabilities/<name>/refresh # 有界刷新；启用 Claude 启动发现后需逐次确认
GET  /api/workspaces?before=<id> # 只读工作区占用 / 共享历史 / 现有保留策略预览
GET  /api/status
GET  /api/tasks?before=<id>      # 最近 100 项，按 id 倒序
POST /api/permission-challenge  # 显式扩大权限前读取并核对绑定范围
POST /api/tasks                 # {key,job:{repository,requirements,agent,test,publish},permission_challenge?}
GET  /api/tasks/<id>
POST /api/tasks/<id>/cancel
POST /api/tasks/<id>/replacement-challenge # 更换停止角色时的权限范围预览
POST /api/tasks/<id>/retry      # 显式核对后继续已停止失败任务，复用原工作区
POST /api/tasks/<id>/continue-review # 同一候选复验一次测试后只继续审查，不重新开发
```

同一 key 与相同规范化 job 返回原任务；同 key 配不同 job 返回 409。发送超时后应保留原 key 重试，避免重复执行。身份口令不是任务 owner；owner 仅是数据库一致性标识。

## 接入真实开发 Agent

把允许访问的本地仓库、Agent、测试和 PR 命令显式写入配置。请求只引用配置中的名字，不能指定任意程序、路径或 shell。原生 `native_agents` 支持 Codex / Claude CLI 的有界 JSONL 协议与终态校验；可显式开启 [Codex app-server / Claude 会话续接](docs/session-continuity.md)；旧通用命令保持兼容。`relay-app doctor <config.json>` 可只探测版本/参数，不调用模型。网页中的[主机能力与模型目录](docs/agent-capabilities.md)提供显式刷新和已知/未知能力说明；目录可见不代表调用授权；首次提交可按宿主允许范围[分别选择开发与审查角色](docs/native-role-selection.md)。严格 Claude 审查保持默认；可单独配置默认关闭的 [Codex 原生本地沙箱审查](docs/native-sandboxed-review.md)，允许命令、显式信任原生集成并逐次确认。模型与供应商凭据由这些外部 CLI 自行管理，Relay 不存储或代办凭据。命令失败和测试失败作为开发结果保存，内核不理解业务状态。

可选命名 `workflows` 将开发 → 测试 → 只读审查 → 有界修复绑定同一 candidate SHA；每次修复必须重新测试和审查。失败后可 [显式继续保留的工作](docs/workspace-continuation.md)，或在已证明的停止阶段[更换当前角色](docs/adapter-stage-continuation.md)，使用新任务编号但复用固定目录；成功工作区可选按期限清理；网页提供[只读保留预览](docs/workspace-inventory.md)，区分已分配空间、共享历史和当前 TTL 证据，不提供删除操作。GitHub 示例仅推送已批准的精确候选，默认 dry-run。

审查接受完整 JSON 或单个完整 `json` 代码围栏，总计最多 4096 UTF-8 字节；512 字节仅用于阶段预览。含外围说明的历史响应不会自动获批；可信操作员可通过本机 `adopt-review` 命令显式采纳一份已完整核对的响应，保留原文/摘要及失败记录，并明确接受原测试证据，不调用模型或重跑测试。条件和风险见[操作说明](docs/workspace-continuation.md#显式采纳已检查的历史审查)。

首次本机安装、真实 CLI 完整配置、停止/重启与备份升级见 [Linux 操作手册](docs/operator-guide.md)。配置、占位符、工作区限制、可选 draft PR 流程及恢复方法见 [运行说明](docs/application.md)。首次接真实 CLI 前先确认其当前版本的调用参数、权限与费用。演示配置是可直接运行的契约示例，不假设你已安装任何模型 CLI。

## 可靠性边界

- 内核最大 payload 64 KiB、result 16 KiB；key / owner 1–128 UTF-8 字节
- 同一个数据库只有一个活跃 claim；每次领取增加 generation，过期执行器不能覆盖新结果
- 取消是请求：只有宿主确认本次进程树已结束，才写入取消结果并释放串行槽位
- 服务重启不会推断旧进程已经停止；未知 claim 保持占用并要求人工核对
- 独立工作区和 Linux supervisor 是可信本机程序的生命周期管理，不是防恶意代码的安全沙箱，也不保证外部副作用恰好一次
- 当前不提供多租户、分布式调度、动态插件、自动部署或生产环境强隔离

## 开发验证

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
python3 -m unittest discover -s app/tests -p '*_test.py'
node app/tests/ui_logic_test.cjs
```

[架构与边界](docs/architecture.md) · [内核设计](docs/adr/0001-local-durable-core.md) · [应用运行说明](docs/application.md)
