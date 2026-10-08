# 精确候选的显式异步合并授权

已发布候选可以在任务页一次确认目标、CI 来源、合并方式和有效窗口，然后由独立 worker 等待检查并提交合并。外部调度 Agent 可使用相同 MCP 接口。宿主默认没有 `merge_policies`；配置策略只是允许选项，每个具体 PR 仍需保存明确授权。

## 必须理解的边界

- GitHub 请求锁定候选 HEAD，并明确发送 `bypass_rules: false`；保护规则由 GitHub 当时执行，不要求关闭保护
- 基础分支、堆叠、队列和 draft 状态只能在请求前核对，不能与 HEAD 一起原子锁定；并发变化仍有风险
- 已发现的堆叠 PR、合并队列和既有自动合并会被拒绝，不主动加入队列或启用 GitHub auto-merge
- 有效期是**最晚发出写请求的时间**，不是完成期限。已经发出的请求可能在过期或撤销后完成；GitHub 未公布该异步请求的撤销接口
- `accepted` / `pending` 仅表示异步请求已登记或仍在处理，绝不表示已经合并
- Relay 不另行删除分支、部署或修改仓库设置；合并可能触发仓库已有自动化，包括仓库配置的分支删除

页面在确认前显示完整风险说明和精确范围。不要将“CI 通过”、预先配置策略或调度 Agent 自己的判断当成用户授权。

## 宿主配置

先按[CI 跟踪说明](exact-head-ci-tracking.md)配置可信来源，再增加可选策略。路径和策略名仅为占位：

```json
{
  "merge_policies": {
    "project-squash": {
      "ci_policy": "project-ci",
      "adapter": {
        "program": "/usr/bin/python3",
        "args": ["/absolute/path/to/Relay/examples/github-merge.py"],
        "env": {"RELAY_GH_PROGRAM": "/usr/bin/gh"}
      },
      "merge_method": "squash",
      "allow_ready": true,
      "target_guard": "preflight_only",
      "authorization_window_seconds": 3600,
      "poll_interval_seconds": 60
    }
  }
}
```

支持 `merge` / `squash` / `rebase`，窗口 60–86400 秒，轮询 60–3600 秒。`allow_ready` 默认关闭；开启只是允许用户在本次确认中选择转正 draft。转正可以先于 CI 通过，使 draft 限制的工作流有机会运行；合并前仍必须重新核对全部指定检查。

示例复用旁边的 `github-ci-observe.py`，两文件应一起安装。程序和已有认证由可信宿主管理，Relay 不安装 gh、不登录、不保存凭据或扩大权限。自定义 adapter 是可信本机命令，不是安全沙箱。策略摘要绑定命令配置和可执行文件身份；参数指向的脚本、依赖及外部认证不因此成为不可变快照，操作员须保护这些宿主文件。

## 一次确认与持久状态

授权绑定成功发布收据、已有 CI 跟踪的可信数字仓库/PR 身份、仓库路径、PR、HEAD、分支、CI 来源、策略摘要、方式和固定期限。用户不需要手工输入 SHA。第一次新鲜远端观测将 GraphQL node ID 与同一个数字 PR 交叉验证后固定；不能根据旧页面或当前配置猜测历史身份。

```text
GET  /api/tasks/<publication-task-id>/merge-preview
GET  /api/tasks/<publication-task-id>/merge-authorizations
POST /api/tasks/<publication-task-id>/authorize-merge
GET  /api/merge-authorizations/<id>
POST /api/merge-authorizations/<id>/revoke
POST /api/merge-authorizations/<id>/reconcile
```

MCP 对应 `relay_merge_preview`、`relay_merge_list`、`relay_authorize_merge`、`relay_merge_get`、`relay_merge_revoke`、`relay_merge_reconcile`。读取只访问本地记录；MCP 本身不启动 worker，仍需运行 `serve`。

创建请求包含完整确认字段、预览摘要、固定期限和幂等键。网络响应未知时重放完全相同的请求，不换键制造第二次授权。撤销和只读核对使用当前 revision；过期页面不能覆盖新状态。历史授权和事件保留，安全的新授权通过前后记录关联，不修改旧同意范围。仅在已证明没有未决副作用时允许新同意：过期/撤销且未发生写入、转正已确认但未发出合并、或同一个异步 UUID 已明确失败；活跃、已受理或未知效果不能借新键绕过。

检查等待、失败、权限不足、缺失来源、候选变化、冲突或不支持的远端状态会显示原因。配置检查尚未结束时持续观察；配置检查已通过但 GitHub 仍报告额外审查/保护阻塞时，本版明确暂停，不无限等待或绕过。处理原因后，只有证明未发生未决写入的记录才能撤销并以新确认重新授权。不会用旧 CI 通过快照直接发出合并。新鲜核对使用当前基础提交并记录本次观察，既不覆盖原 CI 的历史基础提交，也不声称能阻止检查后基础分支变化。

异步请求进入已受理状态后只读查询同一个 UUID。只有可信完成回执才显示 `merged`；只观察到其他操作者已合并会单独表示，不能归功于 Relay。缺失回执、响应截断、超时或未知效果不会触发写请求重试。只读核对也不重新授予写权限。409 返回的既有请求只有选项完全相同时才可只读跟踪，来源标为外部未知；UUID 返回 404 可能只是记录过期，不能据此证明合并失败。

## 独立执行与恢复

合并 worker 不领取内核串行任务 claim，不占住开发槽位，不调用模型，也不保留旧开发工作区。它使用已有私有控制根、进程 supervisor、有界临时目录和持久阶段标记；CI 与合并记录分别管理，内核不增加 GitHub 业务状态。

每次适配器调用最多 30 秒、32 个固定 API 请求、总响应 4 MiB、最终证据 64 KiB。读取完整性不足不能当成成功。唯一合并写入是 GitHub API `2026-03-10` 的 `PUT /repos/{owner}/{repo}/pulls/{number}/merge-async`，参数固定 `direct_merge`、获准方式、精确 SHA 和 `bypass_rules: false`，没有同步/管理员绕过回退。

自定义 merge adapter 必须实现宿主传入的阶段绑定 gate 协议，在每次实际写请求前核对 gate 身份、同意摘要、期限和撤销状态，并在请求期间持有约定文件锁；仅有“可信命令”配置不能代替此协议。参考适配器实现这一约定。

撤销与真实写入前的宿主控制门串行化：撤销先完成时，不再发送该写请求；已经进入发送边界的请求可能完成，必须保留迟到的真实回执。撤销不能撤回已发到 GitHub 的请求。未知本机进程仍需可信操作员核对，HTTP/MCP 不能清除活跃锁。

```sh
relay-app confirm-merge-stopped <config.json> <db-path> <authorization-id> <attempt> <phase> --confirm-process-tree-stopped
```

本机停止证明不等于远端没有副作用。无完整回执时通常继续保持未知，仅允许只读核对。唯一的无写入证明例外是：本机进程已确认停止，且原始 gate 的持久设备/inode、阶段和同意摘要均匹配，并明确证明尚未进入写入边界；缺失、替换或损坏的 gate 不提供此证明。不得仅因为服务重启、期限结束、找不到旧 UUID 或操作员确认进程停止就重新提交合并。

本实现使用离线 API 故障/竞争夹具及本机集成测试验证协议；这些测试不等于已在你的真实受保护仓库中完成合并。首次启用仍需操作员核对 GitHub 当前接口与现有权限，接口不可用时明确暂停，不改用绕过路线。

协议依据：[GitHub Pull requests REST API](https://docs.github.com/en/rest/pulls/pulls)、[GitHub Pull request GraphQL API](https://docs.github.com/en/graphql/reference/pulls)。
