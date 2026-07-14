# 外部 Skill 安装 / 隔离卸载 bridge

适用版本：CSSwitch v0.5.0。该功能只补第三方模型模式下一个缺失的本地工作流，不是通用 Skill Manager。

## 用户合同

安装请求必须提供准确的公开 GitHub 目录 URL，例如：

```text
请安装这个外部 Skill：
https://github.com/owner/repo/tree/ref/path
```

Science 先加载已绑定的 `csswitch-external-skill-tools` routing Skill，再使用自动生成的 `mcp-csswitch-skill-installer` connector。工具名固定为：

- `install_external_skill`
- `uninstall_external_skill`

安装在文件提交后返回 `FILES_COMMITTED_ATTACH_REQUIRED` 与准确 Skill 名；Agent 必须继续调用：

```python
host.agents.attach_skill("OPERON", skill_name)
```

并用 `skill(skill_name)` 成功加载后，才能报告“可用”。卸载在目录隔离后返回 `QUARANTINED_DETACH_REQUIRED`；Agent 必须调用：

```python
host.agents.detach_skill("OPERON", skill_name)
```

并确认 `skill()` 不再加载。不得回退到 `customize`、`host.skills.*`、`skills.deleteDraft`、shell 或手工文件删除。

仅提供名称时，bridge 返回 `NEED_SOURCE_URL` 且不写文件。Agent 可以用正常搜索能力提出候选来源，但不得静默安装一个模糊猜测；bridge 自身不搜索或猜仓库。

## 为什么需要 routing Skill

MCP connection 与 connector 描述本身不能保证自然语言请求选中正确工具。固定 route 提供路由约束，并绑定到默认 `OPERON` Agent；CSSwitch 同时绑定合并 connector、移除受管的旧 uninstaller connector、detach 会误导这类请求的 bundled `customize`，并在保留用户原有文字的前提下维护一段带 begin/end marker 的 OPERON custom prompt。route 不是用户导入 Skill，也不能被外部 Skill uninstaller 删除。

route 使用 `csswitch-system-bridge` ownership marker 原子写入。同名用户内容或被修改内容不会被覆盖。route / connector 注册或 attach 失败只降级该功能，不影响普通 Science 启动。

## 数据流

1. CSSwitch 启动受认证的本地 Gateway，并创建 mode `0700` 的专用 bridge directory；每次 Gateway launch 派生新的 mode `0600` request signing key。
2. 启动 Science 前，CSSwitch 原子合并受管 stdio entry 到 `<data-dir>/mcp/local-mcp.json`，并确保固定 routing Skill；无关 entry 与未知字段保留。
3. Science 将 connector 暴露为自动生成 Skill。MCP 子进程仍在 Science sandbox 中，没有直接写 organization directory 的权限。
4. Science health / identity 确认后，CSSwitch 通过官方 `claude-science url --data-dir ...` 获取一次性本地 URL，使用 loopback nonce / cookie / CSRF 控制流依次 attach 固定 route、attach 合并 connector、移除受管的旧 uninstaller connector、detach `customize`、upsert 并回读受管 custom-prompt block。
5. 用户提出匹配请求后，Agent 加载 route 与 connector，并通过 `request_host_access(mode='rw')` 请求专用 bridge directory。
6. 安装请求为短期、随机 ID、HMAC 签名且绑定当前 Gateway launch；host worker 重新读取 active org，解析 GitHub ref / path，下载完整目录，通过大小 / 路径检查后写入 v1 `.import-origin`，再 no-replace 原子 rename 到 `<data-dir>/orgs/<active-org>/skills/<name>`。
7. Agent 用 Science 原生 attach 并验证 load。
8. 卸载只接受准确名称和有效 CSSwitch `.import-origin`，将完整目录原子移动到 `~/.csswitch/sandbox/skill-trash/`，再由 Agent 原生 detach。

## Agent 控制面所有权与重启行为

上述控制面只接受固定 Agent、固定 route / connector 名和 loopback one-time URL，不是通用 Science 客户端。但是 attach、detach 和 prompt 更新是顺序执行的多个请求，不是原子事务：中途失败会返回 warning，普通 Science 继续启动，先前已经成功的 Agent 配置不会自动回滚。失败时 route-state 不会记为 current，后续显式自检或符合条件的启动会再次 reconcile；报告必须说明可能存在部分配置，不能写成“未做任何修改”。

Science 尚未启动时，CSSwitch 可原子合并 local MCP entry 并确保 route 文件。Science 已在运行时只做只读检查，不并发改写 MCP / route 文件；发现不匹配时返回 `RESTART_REQUIRED`，提示需要重启 Science 才能应用文件配置。

控制面全部成功后，CSSwitch 在 data-dir 中记录不含 secret 的 route-state，绑定 `csswitch_version`、实际 `science_version` 和 `route_revision`。这些值匹配时，普通重复一键启动跳过 attach / readback；首次配置、版本 / revision 变化、registration 变化或显式 self-check 会重新 reconcile。配置失败不会更新成功 marker。

## 安全边界

- 请求必须同 owner、regular file、非 symlink，且在短期内未被修改或 replay；FIFO 等特殊文件被拒绝。
- 限制 redirect、path traversal、symlink、重复目标、文件数、单文件与总大小；安装永不覆盖既有同名目录。
- active org 在处理时重新读取；写入目标只能是 active org 的 Skills 目录，不能是 `<data-dir>/runtime/<version>/`。
- MCP 不绕过 sandbox；不使用 direct database write、私有 bearer、OAuth 模拟、Unix socket 或通用 Science control client。
- host-access denial 对该请求是最终结果，不重试替代路径。
- 控制面可能修改 OPERON route / connector 绑定、detach `customize` 并更新受管 prompt block；只允许修改这些固定对象，不得扩展为任意 Agent 管理。

## 安装证据分层

以下层不能合并成一句“安装成功”：

1. repository / content fetched；
2. 标准 Skill 目录 committed；
3. Science discovered；
4. Agent attached；
5. `skill()` loaded / natural-language triggered；
6. Skill 的脚本、资产、网络、依赖和领域功能完成；
7. Science restart 后仍可用。

v0.5.0 的版本固定 E2E 见 [2026-07-13 调查](../evidence/investigations/2026-07-13-science-runtime-and-skill-bridge.md)。每次 Science App 更新后需重新验证观察到的 `.import-origin`、local MCP、nonce / CSRF endpoint 与 attach / detach 合同。

## 明确非目标

- Anthropic OAuth / catalog 模拟或 Science binary patch；
- Skill Manager、store、inventory、catalog、deployer、同步或通用备份；
- 私有仓库凭证、update / overwrite、永久删除或 quarantine 恢复 UI；
- Python / R 环境与领域依赖管理；
- 通过 `@` artifact / output 替代 Skill registry；
- 保证任一第三方 Skill 的领域功能可用。
