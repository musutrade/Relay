# Codex 原生审查：本地只读，允许命令

这是显式选择的另一档审查能力，默认关闭。现有 Claude `claude_restricted` 严格审查保持不变。新模式 `codex_native_sandboxed_review` **允许命令执行**：Codex 本地命令请求使用 read-only 沙箱、关闭命令网络访问并设置 approval never。它不是严格无执行审查，也不是对整个原生进程或所有工具的全局只读保证。

## 必须先接受的边界

原生 hooks、legacy notify、MCP、Apps、插件和远程执行器可能在模型回合之前启动，或在本地命令沙箱之外执行操作、访问网络和产生费用。只有操作员已信任的原生启动配置和集成适用于此模式。Relay 不认证这些集成、不复制凭据、不改 `CODEX_HOME`、不忽略用户 settings，也不将空 MCP 配置或模型工具模式覆盖伪装成隔离证明。

升级 Relay 不会启用此模式。配置只是允许选择；每次新提交仍须明确选择原生审查模式，读取绑定双方角色、模型、effort、原生模式及宿主配置的权限 challenge，再明确确认。本轮开发授权不等于真实主机启用或模型调用授权。原生设置可以在 Relay 配置之外变化，challenge 不锁定这些设置；每次启用前由操作员核对可信集成。

确认文字明确说明：允许 Codex 本地只读沙箱内命令、approval never，原生 hooks/MCP/插件/远程工具不受该本地沙箱约束，操作员信任这些启动配置和集成，且此模式不等同于严格无执行审查。

## 一次配置，逐次选择

首版仅支持 `codex_app_server`，版本固定为 **0.160.1**；其他版本失败关闭，不能用版本号大于下限推定新契约成立。CLI 必须为可信安装。以下是加入完整宿主配置的片段，不是安装或启用命令：

```json
{
  "native_agents": {
    "codex-native-review": {
      "provider": "codex_app_server",
      "program": "/absolute/path/codex",
      "native_permission": "codex_native_sandboxed_review",
      "allowed_permission_modes": ["codex_native_sandboxed_review"]
    }
  }
}
```

在既有 workflow 的 `selectable_reviewers` 加入该名字；仍可保留 Claude 为默认 reviewer。该 profile 专用于审查，不能用作 developer。不要设置 `session_continuity:true`。只添加 allowlist、只选择 profile 或继承默认模式都不能跳过逐次确认。

前台选择“Codex 原生审查（本地只读，允许命令）”后核对风险与服务端范围。HTTP/MCP 使用既有 `role_selections.reviewer`，显式提供 profile、`native_permission:"codex_native_sandboxed_review"`、`confirm_permission_expansion:true`，并在提交外层携带 `permission_challenge`。该 attestation 字段沿用既有协议，表示明确接受另一档审查能力；不表示更改了宿主系统权限。更改 profile/model/effort/mode 后重新核对；精确已接受重放保持原请求，不消费新的范围。

模型使用该 profile 原生 app-server 返回的新鲜 `model/list` 目录，或明确未验证的手动模型 ID。Astra 的 ID 和 effort 不硬编码，不改写模型的工具元数据。目录可见不等于账户授权或实际执行证据。

## 执行与证据

- 每次调用发送 `thread/start`，请求 `ephemeral:true`、`sandbox:"read-only"`、`approvalPolicy:"never"`。不发送 resume/fork，不复用开发者或旧 reviewer 会话
- 只有响应确认 ephemeral、完整 `{type:"readOnly",networkAccess:false}` 和 approval never，才发送包含审查提示的 `turn/start`；缺失、未知、截断或更宽的策略失败关闭。回合再次显式发送相同本地策略；收到原生设置更新时，策略、网络开关或候选目录缺失/漂移同样停止。该响应是原生会话配置快照，不是 OS 强隔离或每个工具实际行为的证明
- Relay 不自动批准服务端请求。权限、输入或动态客户端工具请求一律不授权，并终止该调用；不会改为 Auto-review、Full access 或其他模式重试
- 使用独立 `reviewer-repository`，不复制开发者 Git 元数据、未提交文件或会话。保留完整有界 diff（最多 256 KiB）、精确候选 SHA、测试前后与审查后完整性检查、严格有界 JSON verdict，以及原有 supervisor/资源预算
- prompt 限定审查、禁止测试/发布/委派。prompt 是行为要求，不是权限隔离。候选完整性检查发现修改后拒绝结果，但不能防止或撤销外部集成副作用
- 自动修复后的下一次审查、显式 review continuation 和同拓扑角色替换均使用新的原生线程；保留工作区与任务链不等于恢复 provider 会话。旧的会话型 profile 保持其原有失败关闭恢复规则

配置 API 的 `reviewer_supported` 只表示 Relay 的配置组合可被接纳，实际版本和策略仍在执行时检查。`reviewer_contract` 区分 `strict_no_execution`、`native_local_read_only` 与 `unsupported`。能力目录的 `reviewer_isolation` 仍表示严格隔离：Codex 保持 unsupported；独立 `native_reviewer` 说明原生档和未验证的运行状态。`probe.read_only_supported` 继续只报告 Claude 严格契约，不能用于证明本档能力。

## 验证范围与依据

官方 0.160.1 发布包 SHA-256 为 `9226581be592d18f7e7f740a352fdb63aa61e45e39f7eb9b09d3888c84bba33f`，对应 Linux musl 可执行文件 SHA-256 为 `f34a4d2301892ae96c90097786bfe5dc269f187b6f69faf42a7b357b8c081e35`。源码/生成 schema 确认 ephemeral 与 readOnly/networkAccess 字段无需实验 API；这不是对任意自报相同版本二进制的签名认证。

离线假 CLI/协议测试覆盖允许项、明确同意、范围绑定/重放、策略缺失与不匹配、意外审批请求、新会话、候选修改和 SHA/verdict 门禁。它们不证明真实账户、模型、费用、托管策略或 OS 沙箱环境可用。0.160.1 的真实无推理 thread/start 探测在开发容器遇到既有 daemon socket 权限检查，未更改权限或绕过；真实主机执行仍需另行授权验收。

官方依据：[app-server 协议](https://learn.chatgpt.com/docs/app-server)、[启动环境构造](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server/src/lib.rs#L619-L629)、[插件启动任务](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/app-server/src/message_processor.rs#L540-L558)、[配置递归合并](https://github.com/openai/codex/blob/rust-v0.160.1/codex-rs/config/src/merge.rs#L96-L148)。
