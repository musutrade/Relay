# Kiro CLI 接入（可选）

Relay 的 `kiro_cli` 使用官方 **ACP V3**，首版面向开发者角色。原有 Codex、Claude 与通用命令 profile 不变；不会替用户安装、登录、创建密钥、修改 Kiro 设置或启用付费试调用。

## 配置与首次检查

需要 Linux Relay 宿主，以及完整安装、已完成原生认证的 Kiro CLI **2.28.0 或更新版本**。程序路径必须绝对且已存在；`kiro-cli` 的配套 `kiro-cli-chat` 也须能按官方安装方式找到（包括服务的 PATH）。Kiro 自身支持的平台不扩大 Relay 当前 Linux supervisor 的范围。

将以下片段合入操作员自己的 host 配置；不把密钥写进项目或网页请求：

```json
{
  "native_agents": {
    "kiro": {
      "provider": "kiro_cli",
      "program": "/absolute/path/kiro-cli",
      "allowed_permission_modes": ["kiro_workspace_write"]
    }
  }
}
```

- 先在宿主以服务账户完成 Kiro 自己的登录；Relay 不代办登录
- `relay-app doctor <config.json>` 只做有界 `--version` 和 `acp --help`，不发任务，也不检查账户
- 任务前 Relay 运行有界 `whoami`；失败即停止，不启动 ACP 或浏览器登录。原生 CLI 可能刷新已有 token，Relay 不接收、保存或转发其 token。就绪成功不是模型授权或余额证明
- 网页 / HTTP / MCP 选 `kiro` 作为开发 Agent。模型可省略，或输入有界、明确未验证的原生 model ID；不从显示名猜 ID

## 权限边界

固定启动参数为 `acp --agent-engine v3 --auth-method cli`。Relay 不传 `--trust-all-tools`，不使用 `dev-shell` 预设，不提供认证、文件或终端客户端能力。`session/new` 的 `mcpServers: []` 只表示不额外注入客户端 MCP，**不禁用 Kiro 原生配置里的 MCP**。

默认未选权限模式时，仅保留 Kiro 的既有原生规则；普通写入可能被拒绝。配置中 `allowed_permission_modes` 只开放一个选项，并不自动授予写入。需要编辑时，用户必须在本次开发角色明确选择并确认 `kiro_workspace_write`；Relay 才传原生 `policyPreset: ["edit-workspace"]`。它允许工作区文件编辑，但不是 OS 沙箱，原生 ask / deny 规则仍有更高优先级。测试命令沿用 Relay 自己配置的测试阶段。

任何运行时权限请求都使本次执行失败并停止回收进程；不会选择 allow、静默提升权限或把后续成功消息当作撤销拒绝。无论选择哪种模式，原生 settings、hooks、agent 配置和 MCP 仍可能执行既有操作，必须由操作员信任。需要强隔离时由宿主另行提供。

## 协议与结果

- 一个进程、一个全新 session、一个 prompt，不自动重发提示或读取旧会话
- 握手固定 ACP protocolVersion 1；请求完全写出后才接受对应响应
- 仅接纳同一 session 的回答；终态必须是对应 prompt 的 `end_turn` 成功响应
- JSON-RPC error、权限请求、取消/拒绝/预算终止、非法 JSON/UTF-8、过长帧、残缺输出、错配/重复响应和缺失终态均不会成功
- 常驻进程收到关联终态后，由原有 supervisor 停止并回收进程树；确认停止前不进入测试或释放任务槽位。超时不能证明未知外部操作未发生
- 保存有界摘要与必要的会话/选择信息，不保存原始 ACP 信封或 `whoami` 身份输出。token、费用、实际执行模型缺失时保持未知

首版明确不支持 Kiro 审查者、Relay 原生 session resume、effort、max_turns、max_budget_usd 和启动式模型发现。失败任务仍可显式继续保留的工作区，但 Kiro 开新会话，通过原有有界交接继续工作，不能称为原生会话恢复。审查可继续选择宿主原有合格 reviewer，不会自动替换。

## 验证记录与官方依据

2026-10-07 从官方 stable manifest 获取 Kiro CLI 2.28.0 Linux x86_64 archive，核对 SHA-256 后，仅在隔离的空 HOME/工作目录运行 `--version` 和 `acp --help`。这验证了版本与 V3/auth 参数接口，**不等于真实账户或模型端到端验收**。运行时协议、权限拒绝、超时/取消与进程树回收由无凭据 fake ACP 集成测试覆盖；真实模型 smoke 需在用户另外授权的已认证环境执行。

选择 ACP 是因为官方 headless 页虽然说明 `stream-json`，但未给出足以严格判定终态与会话关联的事件 schema；没有借用旧 Amazon Q 或其他 CLI 的格式。

- [Kiro ACP V3 迁移](https://kiro.dev/docs/cli/v3/acp-migration/)：启动参数、session metadata 与终态
- [Kiro 原生权限](https://kiro.dev/docs/permissions/)：预设、优先级与工作区边界
- [Kiro 认证](https://kiro.dev/docs/getting-started/authentication/)：CLI 自有认证和原生就绪状态
- [Kiro headless](https://kiro.dev/docs/cli/headless/)：非交互、hooks 和结构化输出边界
- [ACP 握手](https://agentclientprotocol.com/protocol/v1/initialization)、[session](https://agentclientprotocol.com/protocol/v1/session-setup)、[prompt](https://agentclientprotocol.com/protocol/v1/prompt-turn)、[权限请求](https://agentclientprotocol.com/protocol/v1/tool-calls)：标准协议
- [官方 KiroCrew 就绪实现](https://github.com/kirodotdev/KiroCrew/blob/main/src/kiro_crew/kiro_prerequisite.py)：只读探测与未登录门禁的实现依据
- [Kiro 下载和使用条款](https://kiro.dev/downloads/)：官方发行物的许可/协议说明
