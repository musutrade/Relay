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

`app/src/resources.rs` 负责宿主元数据估算、带完整性/时间的逻辑用量及有界结构化失败；HTTP/MCP 只公开 allowlist 名称对应的估算和任务操作状态，读取不启动命令。单次 `workspace_quota_bytes` 是宿主上限内的显式选择；默认用满上限，因此默认无提额余量。已停止任务的提额与继续动作共享同一 immediate transaction 预留，首个请求固定后继 payload；工作区绑定验证紧邻前置配额证明，不清理或复制现场、不修改旧结果。操作状态与实际提交共用门禁，执行前仍核对所有权、候选及发布记录。新字段仅是应用 payload/result/宿主记录的增量，省略额度的旧 payload 规范化保持兼容；不新增核心业务状态。

审查失败的显式后继仍使用应用层唯一 continuation 预留和原 workspace claim，不向内核增加状态。调用者只能提供确认与有界审查重点，候选/base/round 从已结束的宿主结果固定；执行时再次核对现场。由于任意测试命令的外部输入无法自动完整证明，审查后继始终显式复验原测试一次，不建立通用缓存/来源平台，不运行开发或修复阶段。审查提示和验收输入不放宽只读工具、精确 SHA、verdict 或发布门禁。

另有本机操作员显式采纳入口，仅适用于成功审查命令后的格式失败。它保留原始响应及 SHA-256、固定前置结果摘要，沿用同一唯一后继和宿主锁，重新核对候选及配置，但不把旧测试视作可自动复用的缓存。旧记录只有阶段预览，无法独立证明完整模型终态；结果明确标为 `operator_attested`，要求操作员确认整份成功响应并接受原宿主测试。原文保留在不可变后继 payload，完整 verdict 存在结果中，不伪造新的模型调用。通用解析仍拒绝带外围说明的文本，内核、权限、profile 和发布选择均不改变。

## 配置能力目录

`app/src/capabilities.rs` 在可信宿主边界内执行有界的原生目录协议，Relay 不发送模型任务提示；CLI 启动钩子与辅助程序仍可能执行操作、访问网络或产生费用。`catalog_cache.rs` 只缓存配置 profile 的结果和 generation；HTTP 读取不启动进程，显式刷新串行执行，配置/可执行文件变化使旧缓存失效。认证与模型实际调用权限保持独立，执行前兼容性检查不依赖缓存。目录与 UI 不改变核心 payload、队列状态或恢复语义。详见[主机能力与模型目录](agent-capabilities.md)。

Claude 独立启动发现由 host profile 的 `allow_startup_discovery` 显式 opt-in，默认关闭且只适用于 Claude；关闭时仍只检查版本 / 帮助，目录保持未知。启用后缓存读取提供 90 秒一次确认令牌，绑定 Relay profile 的宿主设置、可执行文件身份与进程 epoch，但不启动进程；外部原生 settings / 管理策略不由令牌锁定。前端每次先读取当前范围并显示 profile 及服务端风险提示，只有明确确认才 POST；服务端在启动前拒绝缺失、过期、重放或漂移的令牌。确认被接纳后消费；取消、Esc、退出、导航、会话 / 范围变化丢弃前端同意，未知网络结果不自动重试。此门禁属于应用层，不改变任务提交 / 队列状态。

独立发现沿用正常认证、环境、settings 和权限策略，仅发送 `initialize` 控制请求，不发送模型任务提示；不改用 API 认证、`--bare` 或抑制启动行为来冒充原订阅上下文。managed / user hooks、policy / auth helpers、MCP / 插件仍可能执行操作、推理、网络访问并产生费用，逐次确认不等于启动隔离证明。10 秒发现期限、有界进程树回收与原有未知回收门禁保持生效。返回的模型与 effort 只是该启动上下文公布的元数据，不是全部 endpoint 的清单、订阅访问证明或新的权限授权。

对于版本至少 `2.1.291` 且声明 `--input-format` 的 CLI，已授权的正常任务进程仍先完成控制协议初始化，再发送一次原任务提示。原认证、settings、权限、工作区和会话续接不变；旧 CLI 保持文本输入路径。初始化仅提取有界模型元数据，不公开账户资料、凭据或原始控制 payload。

`task_observation` 单独绑定任务、仓库、角色、请求模型 / effort / 原生模式及时间、版本；每个 profile 在进程内最多保留最近一次，独立五分钟陈旧标记，配置 / binary 变化清除。它不推进可选择目录的 generation，也不写入 `catalog.models`。UI 分区显示，允许没有独立目录时查看，且不得把观察提升为新任务的已验证模型选择或权限证据；缺失字段保持未知。自动后台 / 无人值守刷新与无启动副作用保证不在这个逐次确认设计的范围内。此改动不增加核心业务状态或由 Relay 发送的额外模型任务提示。


## 首次提交的独立角色选择

`app/src/selection.rs` 管理有界的 developer/reviewer 选择、一次配置的宿主 profile/mode allowlist、新鲜目录来源核对和未验证手工模型回退。每个角色独立克隆原生 profile 后执行，不能改程序、环境或审查隔离。新增权限扩大须先取得绑定精确角色/当前策略的短期一次 challenge，再显式确认；challenge 不等于人类同意或访问授权。服务端把非敏感解析值与策略/程序身份摘要封入所选 job，排队后漂移拒绝启动，目录缓存不授予访问权。旧字段省略时保持 payload/profile/工作区/会话指纹；已接受任务的精确重放不依赖瞬时目录状态，后续继续仅继承不可变选择。资源估算使用被选中的 reviewer。供应商结果把请求、服务端会话设置、主消息/作用域 reroute 证据分开保存，缺失遥测保持未知；不增加核心业务状态，见[角色选择](native-role-selection.md)。

新的权限范围/准入记录用 BLAKE2s-256 摘要，不存凭据值或原 challenge；`blake2` 原已是 Argon2 的锁定传递依赖，应用新增直接引用，不引入新下载包。旧工作区/会话 FNV 漂移指纹保持不变。Challenge 只在核心提交成功后消费；精确已接受重放先核对原始请求与原确认 reference，再检查当前临时目录/挑战状态，不产生第二个任务。

## 已停止阶段的显式角色替换

`app/src/replacement.rs` 将宿主停止阶段、原轮次/剩余修复预算、独立前置结果/工作区证明与新选择接纳分开。唯一后继预留在 immediate transaction 及工作区 lease 内冻结有界交接、紧邻前置摘要与新角色 epoch；执行前再次检查，未改变角色的接纳和会话继续继承。Job 的持久 role_epochs 不随 continuation 规范化删除，旧会话文件不覆盖；首次会话创建允许标记在 provider 启动前消费。缺少阶段证明、无法完整保留反馈、原策略漂移或 reviewer checkout 拓扑转换均明确拒绝，不修改队列内核或初始化备用目录。详见[阶段替换](adapter-stage-continuation.md)。

## 只读工作区保留预览

`workspaces.rs` 在应用/宿主边界提供有界配置根清单和索引续接链；只读 claim 与锁、持久结果、完成标记及既有成功 TTL 共同给出保护/未知/等待/策略符合的观察，不创建清理动作。完成证明检查与原宿主清理复用，未改变核心状态或成功 TTL opt-in 默认关闭策略。`resources.rs` 在现有 fd 锚定 walker 中增加独立的 allocated-block 计量，不改变逻辑配额；硬链接去重不能证明独占或可回收空间。HTTP/UI 不查询 GitHub，不接受路径或策略写入。详见[工作区保留预览](workspace-inventory.md)。

## 原生本地沙箱审查档

[ADR 0003](adr/0003-native-sandboxed-review-tier.md) 保留严格审查并新增默认关闭的本地只读命令档。角色选择沿用宿主允许项和逐次 challenge；原生 hooks/MCP/插件/远程工具在本地命令沙箱外，由操作员显式信任。app-server 在发送任务前核对完整策略快照，始终使用新 ephemeral 线程。`sessions::reviewer_checkout` 将独立候选副本与 provider 会话保留解耦，资源估算、审查续接、采纳和替换拓扑均使用同一判断。旧恢复语义不改，内核不增加状态；候选事后核对不保证阻止外部副作用。
