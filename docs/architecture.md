# 架构与责任边界

## 任务路径

```text
外部需求 Agent
    ↓ 已整理的需求
外部适配器 / MCP
    ↓ 有界、不透明 payload + 幂等键
Relay 库 / 本地 CLI → SQLite
    ↑ claim / finish
开发执行器
    ↕ 工作区与进程生命周期
可信宿主
```

适配器、Agent、执行器和宿主是责任划分，不要求拆成独立服务。当前仓库实现 Relay 库/CLI，以及独立的 `relay-app`：回环 HTTP/UI、MCP stdio、开发 schema 与可信 Linux supervisor。它们复用同一个内核，不把业务语义放入库。

## 内核做什么

- 持久化任务及有界文本结果
- 用幂等键消除重复提交；相同 key / payload 返回原任务，不同 payload 使用同一 key 会报冲突
- 串行领取队列中的任务，同库最多一个未完成的 claim
- 校验任务 ID、generation 和 owner，拒绝过期执行器写回
- 在明确确认旧执行已停止后，将未完成任务重新排队

SQLite 是唯一状态源。修改状态使用 immediate transaction，将读取前置条件和写入放在同一事务内，避免不同连接同时领取或完成同一代任务。重启后继续读取已提交状态，不依赖进程内队列。

数据库启用 WAL 与 `synchronous=FULL`，以约束和唯一索引保护基本状态及单一活跃 claim。领取选择最早入队的任务；`active` 可以读取当前 claim，用于区分空队列与被占用的串行位置。

`rusqlite` 使用 bundled SQLite；`serde` / `serde_json` 负责数据与 JSON，`thiserror` 负责错误类型。测试用 `tempfile` 隔离本地数据库。这些内核依赖不包含网络服务；独立应用 package 使用 Axum/Tokio 提供显式请求的 HTTP 入口。

## 状态与失败处理

```text
queued --claim--> claimed --finish--> finished
  ↑                  |
  └--确认旧执行停止后重新排队--┘
```

- `queued`：可领取
- `claimed`：一个 owner 持有当前 generation；在明确完成或安全重新排队之前占用全局串行位置
- `finished`：结果已持久化；内核不解释业务成功、失败或审批结论

每次新的领取推进 generation。`finish` 与重新排队均要求匹配当前 claim；状态变更后的陈旧调用不能覆盖新状态。generation 是数据库写入栅栏，不是外部系统的取消机制。

已经完成的任务接受同一 claim、同一 result 的完成重试，返回原结果；不同结果不能覆盖已完成任务。

没有自动 lease 过期或超时重排。失联可能是进程仍在运行、网络分区或宿主故障，单凭等待时间无法证明旧执行停止。选择保留 `claimed`，以可用性换取不主动制造重叠执行。

恢复步骤：

1. 可信宿主识别旧执行及其子进程，确认它们已经停止
2. 宿主检查或清理工作区，核对可能已经发生的外部副作用
3. 使用当前任务 ID、generation、owner 调用 `confirm-stopped-and-requeue`
4. 新执行器重新领取，按任务自身的恢复策略继续

Relay 不承诺外部副作用恰好一次。执行器在副作用成功后、结果落库前崩溃，可能留下未知结果；重做前需要宿主或适配器核对目标系统，必要时使用目标系统提供的幂等机制。

## 不透明契约

payload 是最大 64 KiB 的 UTF-8 文本；result 最大 16 KiB；key 和 owner 为 1–128 字节。内核检查边界和一致性，不解析需求格式，不信任文本中的指令，也不执行代码。外部组件负责 payload schema、版本、目标授权及结果含义。

CLI 仅把文件和参数映射到库调用并输出 JSON。不应让 CLI 演变成第二套状态机；新入口应复用相同库操作和约束。

## 信任模型与非目标

数据库及其父目录受本机账户和文件权限保护。owner 不是秘密；generation 不能抵御能够直接修改数据库的调用方。所有直接访问内核的调用方必须属于同一可信本地域。现有应用的 HTTP 入口校验 bearer token、只绑定回环地址并限制请求体；配置的仓库/Agent 名称由宿主 allowlist 控制。远程访问和强隔离需要独立运维配置，不由本机能力隐含保证。

以下内容不进入内核：GitHub 业务逻辑、模型选择或调用、证据与质量报告策略、开发流程决策、审批系统、工作区管理、进程管理、动态插件、分布式微服务、UI，以及完整 Agent 实现。增加功能时，先确定它属于哪个现有责任边界，而不是默认扩张核心。


## 应用层状态与取消

`app/` 的开发 schema、配置、supervisor 和 transport 不进入内核。内核仅新增最多 100 项的 ID 游标读取，不新增执行/取消状态。应用在同一 SQLite 文件维护自身 cancellation 与 unknown diagnostic 元数据；取消前使用 immediate transaction 读取核心 claim 并写请求，避免不同进程 claim/取消竞争。运行中的取消手柄按完整 id/generation/owner 匹配。

HTTP worker 遇到 host Unknown 或落库失败仍保留 claimed，不按时间重试。host 成功确认进程树结束后，应用将业务 success/failure/cancelled/timed_out 作为不透明 result 调用核心 finish。网页呈现业务结果；MCP 只提供提交/读取，不引入第二个工作流状态机。详见 [运行说明](application.md)。

## 原生 CLI 协议边界

普通任务的快照也由可信宿主初始化独立 Git 边界，使用空模板并禁用 Git hooks；不继承源仓库的元数据或宿主仓库。supervisor 清除继承的 Git 目录重定向并限制向上发现范围。此边界用于可靠的本地 Git 操作，不改变现有沙箱或供应商网络策略。

`app/src/providers.rs` 负责可信原生 profile、固定 CLI 参数与有界 JSONL 终态归一化；supervisor 在排空输出时解析，不从截断日志反推成功。退出码和供应商终态共同决定应用结果。版本/帮助探测不证明账户认证或模型访问可用。供应商、模型与会话信息仍只是核心不透明结果的一部分，不增加核心业务状态或自动重试。

## 精确候选工作流

`app/src/workflow.rs` 在宿主边界内固定干净 Git 基线，按候选提交串联开发、测试、受限只读审查和至多三次修复。测试与审查绑定精确 SHA，任何候选变化都重新验证；发布只推送已批准提交。GitHub 不确定效果作为有界应用诊断保留，不自动重试，也不扩展核心 queued/claimed/finished 状态。共同期限、轮次数、diff、工作区与持久结果预算都由应用/宿主执行。设计理由见 [ADR 0002](adr/0002-reviewed-candidate-workflow.md)。

## 单用户浏览器认证

`app/src/auth.rs` 管理显式 bearer/session/hybrid 模式、Argon2id 凭据验证、有界进程内会话和登录限流。Cookie 的 HTTPS 与 Origin 边界由可信主机配置，不采信转发头。密码由操作员在本机隐藏输入；服务重启、退出或密码轮换撤销会话。认证状态不进入队列库或任务 payload，MCP 仍保持可信本机 stdio 边界。迁移与回退见 [密码登录说明](password-login.md)。

## 会话适配器

`app/src/app_server.rs` 负责 Codex 双向请求/响应、线程与 turn 关联、拒绝服务端权限/输入请求和有界终态校验。`app/src/sessions.rs` 只保存宿主创建的 task/workspace/role/profile 绑定与显式 ID；执行前持久化 in-flight，成功且完整回收后才可续接。Claude 使用显式 `--session-id` / `--resume`。启用续接的 reviewer 使用固定独立 checkout，继续校验精确候选和只读约束。队列内核、人工确认停止后重排以及未知副作用处理均不改变。详见 [会话续接](session-continuity.md)。

## 固定工作区与续接链

`app/src/workspaces.rs` 管理固定根、带 task/generation/owner 的绑定、跨父进程寿命的 supervisor 独占锁及可选成功 TTL。应用在自己的 SQLite 表预留每个失败前置任务的唯一后继，复用核心 submit 幂等性，不增加核心状态。HTTP/MCP 的显式继续校验已结束失败及现场，直接提交不能注入续接元数据。未知 claim 仍走原人工确认停止流程；发布尝试标记阻止盲目重放。所有代码、未提交工作和旧代目录都不因恢复而重新复制或清空。详见 [固定工作区与继续](workspace-continuation.md)。

可信宿主的 `max_workspace_bytes` 可独立配置整项任务的有界逻辑容量；省略沿用输入快照预算。候选、Git 元数据、独立 reviewer 与控制文件共同计费；准入估算和命令前/运行期/结束检查不构成 OS 硬配额或主机全局磁盘预留。详细容量选择见 [应用运行说明](application.md#工作区容量选择)。

审查失败的显式后继仍使用应用层唯一 continuation 预留和原 workspace claim，不向内核增加状态。调用者只能提供确认与有界审查重点，候选/base/round 从已结束的宿主结果固定；执行时再次核对现场。由于任意测试命令的外部输入无法自动完整证明，审查后继始终显式复验原测试一次，不建立通用缓存/来源平台，不运行开发或修复阶段。审查提示和验收输入不放宽只读工具、精确 SHA、verdict 或发布门禁。

另有本机操作员显式采纳入口，仅适用于成功审查命令后的格式失败。它保留原始响应及 SHA-256、固定前置结果摘要，沿用同一唯一后继和宿主锁，重新核对候选及配置，但不把旧测试视作可自动复用的缓存。旧记录只有阶段预览，无法独立证明完整模型终态；结果明确标为 `operator_attested`，要求操作员确认整份成功响应并接受原宿主测试。原文保留在不可变后继 payload，完整 verdict 存在结果中，不伪造新的模型调用。通用解析仍拒绝带外围说明的文本，内核、权限、profile 和发布选择均不改变。
