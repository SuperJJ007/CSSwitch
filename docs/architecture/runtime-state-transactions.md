# 运行时状态与事务

本文描述 CSSwitch-owned 状态、持久化、锁序和失败补偿。外部 runtime 的选择与身份见 [Science runtime 适配合同](science-runtime.md)，command 与 DTO 见 [Desktop 控制面](desktop-control-plane.md)。

## 状态所有权

| 状态 | Source of truth | 持久性 |
|---|---|---|
| Gateway child、launch ID、key fingerprint、launch context | Tauri `AppState` | 进程内 |
| runtime identity、confirmed-stopped token、boot/history refs | Tauri `AppState` | 进程内 |
| runtime version observation cache | `AppState.science_version_cache` | 进程内缓存，不等于 live identity |
| pending cleanup retry set | `AppState.pending_authority_cleanup` | 进程内镜像 |
| profile、active selection、端口、mode、SSH/Codex 设置、path secret | CSSwitch `Config` | 持久 |
| last healthy binding | `Config.runtime_binding` | 持久 |
| in-flight runtime transaction | `Config.runtime_transaction` | 持久 journal |
| recovery snapshot 与 cleanup authority | CSSwitch private manifest | 跨重启 |
| managed runtime launch | CSSwitch receipt + live identity | 持久记录与 live 组合 |
| Skill bundle、SSH bridge、Codex auth/model generation | 各自 CSSwitch private state | 局部事务持久 |
| 外部 runtime 内容与内部状态 | external runtime | CSSwitch 视为不透明 |

跨重启 cleanup 的权威是持久 manifest；`AppState` 中的集合只是重试镜像。任何单一文件、端口或内存字段都不能证明完整运行身份。

## 锁序与并发

取得 `Lifecycle` 的复合操作遵守：

```text
Lifecycle mutex
  -> AppState mutex
    -> config::update mutex
```

- `Lifecycle` 串行化跨多个 CSSwitch 状态源的命令级变更；
- `AppState` 通常短持有，但当前停止路径会跨外部等待持锁，是状态查询阻塞的诊断点；
- `Lifecycle.generation` 使锁外 probe 在 stop、clear 或 switch 后失效；
- `config::update` 只覆盖一次 load-modify-save，不是跨进程 advisory lock；
- config 提交使用受约束的文件打开、临时文件、rename、fsync、提交前复核与回滚。

Skill bundle、Codex auth 与 SSH bridge 还有自己的 lock、CAS 或 sidecar transaction。本地 Skill 安装不取得 `Lifecycle`，而是在用户选择文件前后复核 runtime context，再进入局部提交；因此上面的锁序不是所有 Desktop mutation 的全局锁序。

## 三个阶段域

| 阶段域 | 形态 | 用途 |
|---|---|---|
| operation trace | typed `OperationStage` | 脱敏运行日志与耗时 |
| runtime journal | string `stage` | crash recovery checkpoint |
| frontend DTO | coarse string | 用户可见失败定位 |

三者当前不能无损互相映射。frontend DTO 仍有字符串分类，auto-boot 还会丢失结构化字段。

## 一键开始事务

冷启动或重启的高层顺序：

1. 读取 active profile 与 provider contract，复核端口和必要的 Codex proof；
2. 进入 `Lifecycle`，恢复 CSSwitch 未完成的 journal 或 cleanup；
3. 若用户启用 SSH，完成 CSSwitch-owned preflight；
4. 确认或精确停止 prior managed runtime；
5. 捕获 CSSwitch 明确管理的 recovery projection，并登记持久 cleanup authority；
6. 准备隔离 runtime context 与可选 bridge state；
7. 启动或复用 Gateway，校验 model catalog；
8. 启动外部 runtime，通过 adapter 校验 health、listener、executable、data root 与 managed receipt；
9. best-effort reconcile 可选 bridge；
10. 提交 runtime binding，清除 journal，再打开 UI。

已健康 runtime 的 reuse 分支先确保 Gateway 和 model catalog，再提交 runtime binding，随后 best-effort reconcile 可选 bridge。两个分支的提交顺序不同，不能互相外推。

任何 CSSwitch-managed 外部状态写入前必须完成 recovery projection。runtime 启动前的失败可在身份安全时精确补偿；启动后外部 runtime 可能已改变自身不透明状态，因此只能恢复 CSSwitch 管理的投影，并把无法确认的外部状态标记为不确定。

## profile 与 mode

- `set_active_profile` 只写 selection，不触碰运行态。
- 新选择由下一次一键开始应用。
- mode 切到 official 时先 bump generation，停止受管 runtime/Gateway，再持久化 mode；停机失败不提交。
- 源码中的运行中 profile switch transaction 只用于测试候选，不属于当前 product-reachable 合同。

## 历史选择恢复

frontend 只持有一次性 opaque reference。backend 复核 active profile、port 和 session 后：

1. 精确停止当前受管 runtime；
2. 让 runtime adapter 恢复用户选定的历史状态；
3. 清理一次性 reference；
4. 重新进入一键开始。

真实路径、内部标识与敏感凭证不跨 invoke 边界。

## 停止

`stop_all` 的 CSSwitch 顺序：

1. bump generation，使旧 probe 或启动结果失效；
2. 通过 adapter 精确停止外部 runtime；
3. 无论上一步结果如何都停止 Gateway；
4. 若 runtime 未确认停止，返回部分成功结果。

停止不能只相信外部 CLI 的退出码；必须结合启动时记录、listener identity 和端口真实关闭。身份漂移时 CSSwitch 不应向未知进程发送信号。

## 恢复与 cleanup

CSSwitch 的 recovery snapshot 只包含显式列入管理合同的投影。其他外部 runtime 根目录和内容保持不透明，不递归复制、删除或回滚。

snapshot 在写入受保护投影前登记为 active recovery；只有事务成功、完整补偿或确认不再需要恢复时，才能以 compare-and-swap 转为 cleanup-only 并删除。进程崩溃或身份不确定时保留 manifest，等待下一次安全恢复或人工处置。

## 当前架构缺口

- journal、trace 与 frontend stage 尚无统一 typed source；
- frontend stage 仍有错误字符串分类；
- auto-boot 丢失部分结构化错误字段；
- config 没有跨进程共享锁；
- 停止路径可能长时间持有 `AppState` 锁；
- 本地 Skill 安装第二次 runtime-context 复核后仍可能与 stop 或 switch 交错。
