# 主机 Agent 能力与模型目录

能力面板只列出宿主配置的 `native_agents`，不会扫描整台主机，也不内置模型名称列表。目录是 CLI 公布的能力元数据，不是全部模型端点的穷尽清单或模型调用授权；可见模型不保证当前账户可调用，也不证明某次任务实际使用了它。

## 操作与接口

- `GET /api/capabilities` 只读取缓存，不启动进程。每个 profile 返回 `name`、进程级 `cache_epoch`、`generation`、`stale`、`refreshing`、`catalog`，以及独立的 `task_observation` / `task_observation_stale` 和 `startup_discovery`
- `POST /api/capabilities/<name>/refresh` 显式检查所选 profile 并获取目录，Relay 不发送模型任务提示。未启用启动发现的 profile 沿用无请求体的调用；启用 Claude 启动发现后必须提交下述逐次确认。使用与其他 API 相同的认证和会话请求保护
- 缓存仅存在于服务进程内，五分钟后标记过期；过期不会自动探测。可执行文件或配置变化使旧目录失效，清空旧值并推进 generation。服务重启更换 cache_epoch，前端只在同一 epoch 内比较 generation
- 每个 profile 只保留最近一次已授权 Claude 任务的模型观察，单独计时并在五分钟后标记陈旧。它不推进可选择目录的 generation，不会写入 `catalog.models`。配置或可执行文件变化同时清除观察；服务重启也不保留。页面只在连接、打开面板或手动读取时获得当前缓存，不为此轮询或启动任务
- 整个服务同时最多执行一个目录探测；重复刷新不会排队启动更多进程。另一个 profile 正在探测时返回 409
- 进程清理无法确认时停止后续探测，交由可信宿主核对。不能把超时当成进程已停止的证据

目录包含检查时间、来源、版本、兼容性、认证已知/未知、模型与可用 effort，以及不支持或无法确认的原因。缺失 effort 字段是未知，不能当成“没有 effort”。配置的模型不在目录中也不能据此断言调用一定失败，因为供应商可能支持别名或额外路由。

`selection` 区分配置请求值与实际生效值。仅获取目录不会产生任务，所以 `effective_model` / `effective_effort` 保持未知。现有手工配置的模型标识可以继续使用，但没有发现证据时会标为未验证；目录本身不修改选择；首次提交的独立角色选择见[角色选择说明](native-role-selection.md)。

## 安全与范围

探测使用已配置的 executable 和环境。Relay 不生成、保存或改配凭据，不把认证密钥复制到结果；原生 CLI 的正常认证加载或刷新仍由其管理。Relay 的发现协议不发送模型任务提示；这不能保证 CLI 的启动钩子或辅助程序不会执行操作、发起推理、访问网络或产生费用。返回错误只包含有界的诊断分类，不回传供应商的原始 stderr。启动行为没有被全面抑制或隔离，具体探测上下文和限制必须在目录来源中说明。

当前任务执行仍会重新检查版本和关键参数；能力缓存不会绕过执行前的兼容性、只读审查隔离或工作区/会话绑定检查。Codex reviewer 在完整只读隔离契约获得验证之前保持不支持。此功能不修改核心队列状态，不自动扩大权限，也不启动任务恢复或发布。

## 发现协议与未完成的支持

Codex 使用当前配置 binary 的 app-server `initialize` → `initialized` → 分页 `model/list`，读取运行时返回的模型和 supportedReasoningEfforts。不会发送 thread/start 或 turn/start。它在私有空目录初始化，仍使用原有环境和宿主配置；来源/上下文会明确说明这点。[官方协议](https://learn.chatgpt.com/docs/app-server#list-models-modellist)

### 逐次确认的 Claude 独立启动发现

Claude SDK 的初始化响应可提供 `models`，无需 Relay 先发送模型任务提示。独立启动发现**默认关闭**：宿主必须在对应 `native_agents` 的 Claude profile 上显式设置 `"allow_startup_discovery": true`；此字段仅适用于 Claude，省略或 `false` 保持旧行为。网页只显示该设置，不提供更改 profile、环境、权限或策略的入口。`GET /api/config` 的 `native_agents` 返回该布尔值。

- 未启用时，显式刷新只做原有版本 / 帮助检查，独立模型目录保持未知；正常任务带回的观察保持独立
- 启用时，每次点击刷新先读取最新缓存，然后显示原生确认对话框，明确列出 profile 名称及服务端给出的 `confirmation_text`。取消、Esc、关闭对话框、退出、前进 / 后退、会话变化或确认范围变化均丢弃本次前端同意；必须重新读取并重新确认才能开始
- 确认内容说明：正常 managed / user hooks、policy / auth helpers、MCP / 插件可能运行、访问网络、执行操作并产生费用。Relay 仅发送 `initialize` 控制请求，不发送模型任务提示，**不承诺无副作用、无推理、无网络或无费用**。未受管理或受管理的启动副作用均不会被这次发现全面抑制
- 沿用该 profile 的 binary、环境、正常认证与既有权限策略；不换用官方 API 账号目录、不使用 `--bare`、不通过强制禁用 hooks / MCP 或更换策略来冒充相同启动环境。私有临时启动目录仍可能与实际任务工作区不同，须结合来源与启动上下文阅读结果
- 目录进程继续共用既有 10 秒发现期限、输出预算与有界进程树回收；超时不能证明没有外部影响。只有确认回收后才能释放发现锁，结束状态未知仍保留恢复门禁

启用时的 `startup_discovery` 形如：

```json
{
  "confirmation_token": "opaque-one-time-token",
  "expires_at_unix_ms": 1700000090000,
  "confirmation_text": "Server-provided startup-effects warning for this configured profile"
}
```

禁用时该字段为 `null`。空闲时令牌有效期为 90 秒，在消耗、过期或绑定范围漂移之前读取返回同一令牌；发现运行中 `confirmation_token` / `expires_at_unix_ms` 为 `null`。缓存读取可能生成令牌，但仍不启动任何进程。

只有用户明确点击确认后才提交一次：

```text
POST /api/capabilities/<name>/refresh
Content-Type: application/json
{"confirm_startup_effects":true,"confirmation_token":"opaque-one-time-token"}
```

令牌绑定 Relay 中的 profile 设置（包括宿主启用策略）、可执行文件身份与服务进程 epoch；它不锁定外部原生 settings 或管理策略文件。缺失、过期、重复使用或绑定漂移均在子进程启动前拒绝；新的缓存读取可获取当前范围的令牌。令牌是防陈旧 / 重放约束，不代替用户同意、身份认证或 CLI 的权限策略。确认被接纳后即消费，协议失败或超时不允许旧令牌重放。前端防止重复点击；响应丢失时保留未知状态，先读取缓存核对，不自动重试，也不重用已提交令牌。取消后服务端尚未使用的令牌可能仍相同，但新的点击必须重新呈现对话框并重新确认。

初始化只提取有界允许字段：模型 ID、显示信息、已返回的 effort 与可选能力。它们是该启动上下文公布的元数据，不是所有 endpoint / 路由的清单、订阅权益清单或账户可调用证明。缺失字段保持未知，不从 CLI 版本或名称猜测支持度。

普通 `disableAllHooks` 无法覆盖受管理 hooks；`--bare` 不使用订阅 OAuth 登录或系统钥匙串，因此不能作为保留现有订阅认证的无副作用替代方案。这里并不推断用户机器确实配置了某个 hook，也不证明账户已拒绝访问。[官方非交互与 bare 模式说明](https://code.claude.com/docs/en/headless#start-faster-with-bare-mode) · [官方 hook 优先级](https://code.claude.com/docs/en/hooks#disable-or-remove-hooks)

### 已授权任务内的 Claude 模型观察（部分支持）

对于执行前检查通过、版本至少 `2.1.291` 且帮助中声明 `--input-format` 的 Claude CLI，Relay 在**本来就要执行的已授权任务进程**中使用 `stream-json` 输入：先发送一次 `initialize` 控制请求，再发送一次原有任务提示。没有为了更新面板额外启动的 Claude 初始化进程、额外用户回合或模型试调用。原任务的认证、环境、settings、权限、工作区与 `--session-id` / `--resume` 路径不变；开发者和审查者继续遵守各自已有的权限与隔离契约。旧版 CLI 沿用原文本输入，不为取得观察而增加新调用。[SDK 初始化实现](https://github.com/anthropics/claude-agent-sdk-python/blob/main/src/claude_agent_sdk/_internal/query.py)

`task_observation` 为 `null` 或以下任务作用域信息：

- `task_id`、仓库 allowlist 名称 `repository`、`role`（`developer` / `reviewer`）、`cli_version`、`checked_at_unix_ms`
- 此任务的 `requested_model`、`requested_effort`、`native_permission`，不与 profile 默认值或其他任务混用
- 初始化实际返回并经过有界校验的 `models`；模型 ID 和 effort 来自响应，不内置名称列表

模型条目沿用 `ModelCapability`，并可提供 `resolved_model`、`supports_effort`、`supports_adaptive_thinking`、`supports_fast_mode`、`supports_auto_mode`；初始化的 `supportedEffortLevels` 转为 `supported_efforts`。缺失可选字段仍是未知，`false` 与缺失不同。`resolved_model` 只说明初始化时别名如何解析，不是任务实际调用该模型的证明。字段契约参考官方 [Agent SDK 0.3.291](https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.291) 的 `sdk.d.ts` / `ModelInfo`；以每次实际响应为准，不推测没有返回的能力。

这里只保存并公开上述允许字段，不返回初始化的账户资料、凭据、原始控制 payload 或供应商原始错误。初始化属于现有任务启动行为，不能据此宣称整个任务无网络、无 hooks 或免费。

页面把观察与独立目录分开显示，即使 `catalog: null` 也能看到任务编号、仓库、角色、请求配置、观察时间、CLI 版本和独立陈旧标签。观察只适用于那个任务的初始化上下文，**不是完整实时模型目录、账户可调用证明或授权依据**。观察条目不会进入“已验证目录”的模型 / effort 选择；仍可显式使用未验证的手工 model ID，执行时继续由原生 CLI 验证。

已实现的 Claude 路线包括正常任务内的模型观察，以及宿主 opt-in、每次明确确认后的独立启动发现。自动后台或无人值守刷新、无启动副作用保证不在这项设计范围内。逐次确认不等于隔离证明；不能把一次任务观察标为该 profile 的完整可选目录。

任务观察的时间取初始化响应收到时刻，五分钟新鲜度也从该时刻计算；后续长任务、测试或审查不会重新延长有效期。

## 只有进程状态未知时才需要核对

普通协议失败和已确认结束的超时会自动释放发现锁，不需要更改文件或重启服务。如果服务异常退出或进程树结束无法确认，先由可信宿主核对保留的发现进程与诊断，再执行：

```sh
relay-app doctor <config.json> --confirm-catalog-stopped
```

此命令声明操作员已确认之前的发现进程树停止，仅清除对应的恢复标记，不杀进程、不推断停止、不调用模型。仍在运行并持锁的发现操作会被拒绝；服务随后允许重新刷新，无需手工删除文件。不要在未核对进程时使用这个确认选项。
