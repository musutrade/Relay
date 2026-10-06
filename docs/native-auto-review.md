# Codex 原生 Auto-review（显式可选）

`codex_auto_review` 是开发角色的原生权限选项，与开发模型、代码审查角色及代码审查模型分开。Relay 复用 Codex 自带的审批机制，不创建审批分类器，也不承诺某个开发或审查模型会成为审批模型。

## 准入与边界

- 仅适用于 `codex_app_server` 开发角色；`codex_cli` 的 exec 路径没有同等的会话设置回传证据，本次不开放
- 可信宿主必须先把 `codex_auto_review` 加入该 profile 的 `allowed_permission_modes`；现有默认模式、主机配置和凭据不会自动改变
- 操作员仍须在网页或 HTTP/MCP 客户端明确选择模式，取得绑定精确角色、模型、策略及可执行文件的短期 challenge，并确认后提交。变更选择或宿主策略须重新确认；已接纳的精确幂等重放和续接沿用原不可变选择
- 请求固定为 `workspace-write` + `approvalPolicy: "on-request"` + `approvalsReviewer: "auto_review"`。start / resume 通过原生临时配置覆盖请求同一工作区 baseline；turn 明确请求仅当前工作区可写、`networkAccess: false`（保留原生临时目录默认值）；符合条件的文件、网络和工具越界请求仍可能由原生审批器自动批准，因此不能承诺绝对只读或所有网络访问被禁止
- Auto-review 不是任意访问许可，也不是强隔离保证。托管策略、原生配置、账户、供应商与运行时能力仍可能拒绝；启动钩子、MCP 和远程工具属于可信原生运行环境，不能由本地 sandbox 字段推断其安全性
- 只读代码审查角色不能选用这个模式，已有审查契约不变

宿主配置片段（仅说明配置项，不会由 Relay 自动写入）：

```json
{
  "provider": "codex_app_server",
  "program": "/absolute/path/to/codex",
  "allowed_permission_modes": ["codex_auto_review"]
}
```

## 验证与结果

沿用 app-server 的版本 / help 门禁（当前至少 0.160.0），每次启动与续接都明确传递模式。版本检查本身不代表账户支持 Auto-review。发送任务 turn 之前，必须从关联的 `thread/start` 或 `thread/resume` 响应观察到所选 cwd、workspace-write 完整基线（networkAccess 为 false、writableRoots 仅当前 cwd 或原生隐含 cwd 的空数组、两个临时目录排除项均 false）、on-request 和 auto_review；不完整回传保持 `unknown` 并停止，不把请求值冒充实际值。冲突值标为 `mismatch` 并停止，不切换 full access、人工批准或其他供应商。执行中的 `thread/settings/updated` 会重新核对完整原生基线。已发现失败时，不再发送仍排队或部分缓冲的任务提示。

结果分别保存请求的 `native_permission` / `approvals_reviewer`、原生返回的 `session_settings.approvals_reviewer`、sandbox / approval policy 以及验证状态。会话设置不是每个工具执行的强制约束证明，也不证明具体审批模型的身份。缺少真实执行证据时，不声称账户、模型或审批已验收。

当原生协议提供关联的 `item/autoApprovalReview/completed` 通知时，保留最近最多 8 项有界审批状态、动作类型与理由。拒绝、超时或中止不由 Relay 转成批准，也不触发 Relay 重试。沿用现有 fail-closed 处理：原生若报告工具 `status: declined`，Relay 停止本次执行，保留拒绝记录；因此本适配器不保证原生 Agent 能在该次工具拒绝后继续寻找替代方案。原生需要直接向客户端询问批准、凭据或用户输入时，Relay 仍拒绝并停止，关闭 stdin；不会为了发出错误应答而继续发送已排队的提示。审批完成记录只证明原生报告了该决定，不证明获批操作实际执行，也不改变任务的终态门禁。

## 本次验证范围

协议 fake 覆盖请求参数、启动 / 恢复回传缺失或漂移、原生拒绝与客户端批准回退、确认范围变更和旧记录兼容。公开 schema 的本地无模型探测确认了 approvalsReviewer 字段；未启用真实宿主配置、发起真实模型任务或证明任一账户的 Auto-review 可用性。

官方语义：[Codex Auto-review](https://learn.chatgpt.com/docs/sandboxing/auto-review)、[App-server](https://learn.chatgpt.com/docs/app-server)
