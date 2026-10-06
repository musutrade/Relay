# 首次提交时分别选择开发与审查 Agent

角色选择属于 `relay-app`，不进入不透明队列内核。先由可信宿主一次性配置可用 profile；之后每次提交可在已允许的范围内选择，不必反复编辑本机配置或手工启动 Agent。profile 名字决定已配置的适配器和程序，HTTP/MCP 不接受可执行文件、环境变量、任意参数或配置文件路径。

## 宿主允许范围

`workflows` 可增加 `selectable_developers` / `selectable_reviewers` 名字数组，各最多 64 个、不能重复。固定的 `developer` / `reviewer` 始终保留为默认允许项；省略数组仅允许原来的固定项。仓库、测试、Git、修复次数和发布目标继续由工作流固定。

```json
{
  "repository": "project",
  "developer": "codex",
  "reviewer": "claude-reviewer",
  "selectable_developers": ["claude-developer"],
  "selectable_reviewers": ["claude-reviewer-alternate"],
  "test": "check"
}
```

原生 profile 可增加 `allowed_permission_modes`，只允许该适配器支持的有界模式。省略它不会扩大权限。可选 `native_permission` 指定该 profile 的默认模式；默认模式若扩大权限，提交时仍须显式确认，不能通过继承规避确认。

开发者模式：

- `codex_workspace_write`：workspace-write 与 approval never
- `codex_full_access`：danger-full-access 与 approval never，同时扩大文件系统和网络访问；需宿主允许并逐次显式确认
- `claude_dont_ask`：原生 dontAsk；需宿主允许并确认
- `claude_auto`：原生分类器 auto；需宿主允许并确认。模型、供应商、版本、账户与托管策略适用性仍可能未知，不能把可选理解为已生效，也不会失败后换成 bypass
- `claude_bypass_permissions`：原生 bypassPermissions；需宿主允许并确认，明确提示文件系统和网络访问扩大

严格审查者使用 `claude_restricted`，它是 Relay 的固定只读审查契约，不是 Claude 的 `--permission-mode` 值。它保留 restricted、Read/Glob/Grep、执行/委派禁用以及 MCP/自定义命令限制。不能把开发者或 Full access 模式用于审查。Claude 各模式仍带 `--permission-prompts none`；未回答的请求会被拒绝，不自动批准。Relay 不支持交互审批或输入请求。

严格 Codex reviewer 仍保持 `review_profile_unsupported`：完整的版本验证、启动/配置加载、hooks/MCP、无执行工具、恢复隔离契约尚未证明。这不表示 Codex 没有关闭单项 hooks/MCP 的控制。Codex/astra 审查支持仍是未完成验收项，不能把当前角色选择子集当作已完成它。

#22 的 Codex/astra 后续验收仍需覆盖：固定已核验的版本/实验协议 schema；启动前的 plugin、remote environment、legacy notify 与配置行为；Astra 模型元数据覆盖工具模式开关；动态 MCP/托管配置刷新缺少原子工具上限；fresh/warm/cold resume 都保持无执行工具。上游内部 `ToolPolicy.allowed_tools=[]` 不是公开 CLI/RPC 契约。需要真实工具 schema 的空集合断言与 sentinel 测试，不能以一次宿主声明或单项禁用标志替代证明；当前实现不引入这些未验证路径。

另可显式选择默认关闭的 [`codex_native_sandboxed_review`](native-sandboxed-review.md)：本地只读、允许命令、approval never，原生启动与外部集成由操作员信任，需宿主允许并逐次确认。首版仅 app-server 0.160.1，使用新独立线程；不替代严格模式。

## 提交 schema

新增可选 `job.role_selections`，分别含 `developer` 与 `reviewer`；至少一个存在。`job.agent` 必须等于所选 developer 的 profile。没有工作流不能选择 reviewer。只改 profile 时可仅提供名字；generic 命令 profile 仅支持开发者的 profile 选择。

```json
{
  "key": "stable-submission-key",
  "job": {
    "repository": "project",
    "requirements": "实现需求并补测试",
    "agent": "codex",
    "workflow": "reviewed",
    "role_selections": {
      "developer": {
        "profile": "codex",
        "model": {"value": "operator-supplied-model-id", "source": "manual"},
        "native_permission": "codex_workspace_write"
      },
      "reviewer": {
        "profile": "claude-reviewer",
        "model": {"value": "operator-supplied-review-model-id", "source": "manual"},
        "native_permission": "claude_restricted"
      }
    }
  }
}
```

这里的模型名是 schema 占位示例，不是内置目录或可用性承诺。模型与 effort 限 1–256 UTF-8 字节，禁止控制字符与开头的 `-`。Codex effort 通过安全转义的配置字符串传入，不将目录文本直接插入 TOML。

目录模型用 `model: {value, source:"catalog", catalog:{cache_epoch,generation}}`，必须匹配当前 profile 的新鲜、完整目录及 `model` 值。选择 `effort` 还必须匹配该模型的 `supported_efforts`，不使用供应商通用枚举。缺失 effort 元数据时不能新增 effort 覆盖；可信 profile 的原有值可保留，但不因此声称已验证。`source:"manual"` 是明确未验证的模型回退，不能携带伪造目录引用，也不生成 effort 选项。

扩大权限必须先调用 `POST /api/permission-challenge {job}`（MCP：`relay_permission_challenge`）。预览可以不含确认、需求文本可以留空，但扩大权限的 `native_permission` 必须明确填写，不能仅继承可变默认值。服务返回 `challenge`、`expires_at_unix_ms`、`confirmation_text` 与不含凭据的 `scope`。界面先显示实际 profile、适配器、模型、effort 与原生模式，再由操作员确认；Full access / bypass 明确写出“扩大文件系统和网络访问”。

正式提交在外层 `{key,job,permission_challenge}` 携带 challenge，并在角色内提供 `confirm_permission_expansion:true`。Challenge 本身不是人类同意的证明，只绑定已核对的范围；显式 attestation 仍必需。它是 5 分钟、一次使用、进程内最多保留 64 个的随机标识，绑定角色选择、模型来源/目录引用、仓库/工作流和当前宿主策略。角色、模型、effort、模式或目标变化需要重新核对；单纯编辑需求文本不改变权限范围。它不会越过宿主 allowlist、供应商策略或审查限制，也不授予目录探测或真实执行权限。

只有核心提交成功才消费 challenge；数据库失败没有创建任务时仍可重试。未接受的 challenge 在服务重启后失效；已接受请求保留原 key、job 和 challenge 重放，即使 challenge 已过期或服务已重启也返回原任务。换成新 challenge 是不同的确认，不能用同一 key 把改变后的默认值或策略伪装成旧意图。

## 幂等、绑定与恢复

省略新字段的旧 job、profile、工作区和会话指纹保持原有序列化。首次提交核对动态目录和必要的权限 challenge；已接受原始请求及其确认的精确重放在目录/确认过期或服务重启后仍返回旧任务，不同内容或不同确认保持冲突。请求未知时保留原 key 和 payload，不能悄悄刷新目录引用再重试。

每个带角色选择的已接受 job 增加服务器专用 `role_binding`，固定解析后的非敏感请求设置，以及宿主 profile/权限策略/可执行文件身份的 BLAKE2s-256 摘要。扩大权限还保存原 challenge 的单向 acceptance reference，不保存 challenge 原文、程序环境或凭据值；客户端不能填写该字段。排队之后宿主 profile、默认模型/effort、策略或可执行文件发生变化时，启动及继续检查会拒绝，不采用新默认值。供应商内部设置和真正执行值仍只能按实际返回证据解释。

两个角色独立解析到可信 profile 的克隆，允许同一 profile 的两个角色选不同模型而互不覆盖。解析后的模型、effort、权限与原程序/环境一起进入工作区和会话绑定。`retry` / `continue-review` 默认只继承原选择；可在服务端证明的停止阶段显式[更换当前角色](adapter-stage-continuation.md)，保持原工作区、原轮次和独立会话 epoch。候选 SHA、测试复验、独立 reviewer 会话、未知进程、发布核对与资源配额门禁不改变。

`GET /api/resources` 和 MCP `relay_resources` 可带 `reviewer_profile`，必须属于所选 workflow 的允许项。估算使用该 reviewer 的会话/独立副本要求；界面切换 reviewer 时应使旧估算失效。

## 请求值、会话配置与执行证据

`provider.selection` 与每轮 developer/reviewer 的 `selection` 分开保存：

- `requested`：请求的 profile、适配器、model、effort、native_permission、模型来源
- `session_settings`：供应商实际返回的会话配置及来源；缺失字段为 null
- `observed`：主会话 assistant 消息模型与带 thread/turn ID 的模型 reroute 证据
- `verification`：unknown / session_reported / message_reported / rerouted / mismatch，只说明证据范围，不承诺所有调用生效

Codex thread/start 或 resume 返回的 model、reasoningEffort、approvalPolicy、sandbox 是会话配置快照，不是某个 turn 的执行遥测。当前 effort 在随后 turn/start 发送，因此之前的 reasoningEffort 不能证明该覆盖值。turn/start 没有有效 model/effort 回显；Codex exec 的 thread.started 也没有这种证明。正确作用域的 model/rerouted 保留 from/to，不能断言初始模型处理了所有请求。

Claude system/init 的 model、permissionMode 和可选 effort 属于会话配置；assistant.message.model 属于消息证据，带非空 parent_tool_use_id 的子 Agent 消息不能覆盖主角色模型。缺失字段保持未知。原生权限拒绝或明确模式不匹配会失败并保留原因，不静默降级。`claude_restricted` 不与原生 permissionMode 比较。

所有证据有界，并共享既有 16 KiB 结果预算；压缩以 `truncated` / `evidence_truncated` 标示。能力目录不是认证、授权、账户权益或实际执行证据。Claude 自动模型发现的受管理启动隔离仍未验证，见[能力目录说明](agent-capabilities.md)。

Codex app-server 开发角色另可由宿主显式允许 [原生 Auto-review](native-auto-review.md)：`codex_auto_review` 使用 workspace-write + on-request + 原生 auto_review，须单独确认可能获批的文件、网络和工具越界。它与代码审查模型分开，不适用于只读 reviewer，也不代表某个账户已具备原生自动审批能力。
