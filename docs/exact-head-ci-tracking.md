# 已发布候选的确定性 CI 跟踪

成功发布真实 draft PR 后，可以在任务页面选择宿主配置的 CI 策略并启动跟踪，也可以由外部调度 Agent 通过 MCP 操作。跟踪固定已发布的 PR、候选 SHA、分支和检查来源；不重新开发、测试或审查，不调用模型，也不改变 PR。

**“已配置检查通过”不是“GitHub 允许合并”。** 此阶段始终返回 `remote_merge_eligibility: "not_established"`，不会转正 draft、合并或部署。GitHub 保护规则、审查要求及即时合并授权属于后续独立步骤。

## 配置一次，按任务启动

宿主可以增加命名 `ci_policies`。以下仓库、程序路径和数值编号只是占位，须换成操作员核对过的真实配置：

```json
{
  "ci_policies": {
    "project-ci": {
      "github_repository": "example/project",
      "base_branch": "main",
      "observer": {
        "program": "/usr/bin/python3",
        "args": ["/absolute/path/to/Relay/examples/github-ci-observe.py"],
        "env": {"RELAY_GH_PROGRAM": "/usr/bin/gh"}
      },
      "workflow_id": 123456789,
      "app_id": 98765,
      "event": "pull_request",
      "required_jobs": ["check", "browser"],
      "poll_interval_seconds": 60,
      "observation_window_seconds": 86400
    }
  }
}
```

策略只支持指定 GitHub Actions 来源，不是任意检查平台。检查名单必须非空且没有重复；名称、数字 workflow/app ID 和事件共同定义可信来源。仅按同名 job 或同一个 GitHub App 匹配不足以证明来源，因为其他工作流也可能使用同名检查。

当前策略支持 1–8 个精确 job 名，轮询间隔 60–3600 秒，窗口 60–86400 秒。参考观察器每轮最多 24 个固定 GET 请求，每个分页集合最多 3 页，单响应 512 KiB、总响应 2 MiB、最终观测 64 KiB；宿主每次最多运行 30 秒。触及预算明确报告不完整，不能截取部分成功记录作为通过。

程序及认证仍由可信宿主管理。示例观察器只发送固定 GitHub GET 请求，不登录、不保存凭据、不重跑 CI。自定义 observer 仍是具有本机账户权限的可信配置命令，不应将它理解为安全沙箱。数据库保存公开来源、策略摘要和有界观测，不复制 observer 环境变量、参数或凭据。

启动只接受已有成功真实发布收据；dry-run、未知发布或缺失精确身份不能启动。新收据记录目标基础分支；旧 publish-approved 记录可从不可变交接请求取得它，旧直接发布记录没有此字段时明确拒绝，不用当前配置猜测过去目标。

## 使用与状态

页面读取本地预览后直接选择策略并启动，不需要手工输入 SHA 或额外逐步确认。启动请求自动携带策略摘要与幂等键；网络响应未知时保留相同请求。停止/继续携带当前 revision，避免旧页面覆盖较新的状态。

主要接口：

```text
GET  /api/tasks/<publication-task-id>/ci-preview
GET  /api/tasks/<publication-task-id>/ci-tracks
POST /api/tasks/<publication-task-id>/track-ci
GET  /api/ci-tracks/<id>
POST /api/ci-tracks/<id>/stop
POST /api/ci-tracks/<id>/resume
```

MCP 对应 `relay_ci_preview`、`relay_track_ci`、`relay_ci_get`、`relay_ci_stop`、`relay_ci_resume`。读取接口只读取本地持久状态，不查询 GitHub，也不启动进程；MCP 本身不启动后台 worker，仍需运行 `serve`。

- `watching`：等待该来源的检查出现或结束；缺失检查不会被当作通过
- `configured_checks_passed`：配置项在本次完整、同 SHA、同可信来源的观测中全部成功；它是带时间的历史观测
- `checks_failed`：检查有明确失败；在 GitHub 修复或重跑后，可以显式继续观察同一 SHA
- `blocked`：身份、来源、策略、权限或协议等无法证明，显示具体原因；不会跳过门禁
- `process_unknown`：不能证明观察进程树已停止，仅能由可信本机操作员核对
- `expired` / `stopped` / `pr_closed` / `pr_merged`：当前窗口或远端 PR 已结束；远端关闭/合并只报告观测，不表示 Relay 执行了操作

默认每 60 秒进行一次短观察，单个窗口最多 24 小时。已确认停止的记录可显式开始新的有界窗口，仍保留原 PR、SHA、来源和数字身份，不必重新审核或发布；重复同一控制请求不能重复延长窗口。未知进程不因过期或点击继续而自动恢复。

继续仅适用于 `checks_failed`、`blocked`、`stopped`、`expired`；通过、已关闭或已合并记录保留终态，未知进程须先完成本机核对。继续不会接受新的远端 HEAD、基础提交或数字身份。

## 精确来源与失败边界

观察器核对 PR 仓库、分支和 SHA，再读取指定 workflow 的最新 run/attempt，通过该 attempt 的 job、check-run URL/ID、app、head SHA 和 check-suite 建立关联。结束前复读关键身份，拒绝把旧成功、新失败或另一个来源的同名检查混为一谈。

只有明确 `success` 才通过；`skipped`、`neutral`、`cancelled` 不自动算绿。分页未完成、输出截断、重复或不明来源、身份漂移都不能变成通过。正常检查状态推进可以在原窗口内重新观察；已确认回收进程后的短暂网络/限流/服务故障最多重试三次，之后明确暂停，认证或权限不足不会触发自动登录或权限扩大。

第一次可信远端观测固定数字仓库/PR ID；后续不得偷偷换目标。首次观测的基础提交单独保存；后续漂移明确暂停，不声称能原子固定 GitHub 基础提交。规则或基础分支并发变化仍需后续合并步骤重新核对。

## 独立 worker 与宿主恢复

CI worker 在应用自己的持久记录中运行，不领取队列内核的串行 claim。因此远端 CI 等待期间，开发队列可以继续执行。它不依赖旧开发工作区，不延长该工作区的保留期限，不覆盖原任务结果。

控制文件位于 `workspace_root/.ci-tracking/<canonical-db-digest>`。只有精确验证过的私有宿主控制目录被排除在任务工作区保留数量之外；其他未知文件和旧工作区门禁保持不变。每轮临时目录最多 1 MiB / 128 项，不与任务目录混用。数据库持久绑定控制根的路径、设备及 inode；根配置或目录身份发生漂移时，仅阻止 CI 操作并要求恢复原现场，不迁移或绕开旧的活跃锁。历史记录仍可读取。

每轮使用独立私有目录、有限时间/输出和持久所有权标记，锁由现有 supervisor 继承。只有进程树已确认回收且对应版本的结果已经落库，才移除本轮标记。崩溃、未知回收或落库失败都保留标记；服务重启或时间流逝不能证明旧进程已经停止。

操作员核对后可在本机执行：

```sh
relay-app confirm-ci-stopped <config.json> <db-path> <track-id> <attempt> --confirm-process-tree-stopped
```

仍被进程持有的锁、错误 track/attempt 或无法验证的现场会拒绝。确认保留恢复诊断，之后再显式继续；HTTP/MCP 不能清除未知所有权。CI lane 的未知状态只阻塞 CI 观察，不占用开发队列 claim。

协议依据：[Workflow runs](https://docs.github.com/en/rest/actions/workflow-runs)、[Workflow jobs](https://docs.github.com/en/rest/actions/workflow-jobs)、[Check runs](https://docs.github.com/en/rest/checks/runs)。
