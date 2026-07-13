---
name: csswitch-external-skill-tools
description: "CSSwitch 外部 Skill 安装与卸载的强制路由。MUST use in CSSwitch-managed Claude Science whenever the user asks to install, import, add, uninstall, remove, or delete an external Skill."
---

# CSSwitch external Skill tools

This Skill only routes external Skill operations. It is not an authoring or
publishing workflow. For every request in scope, use the matching CSSwitch MCP
connector and do not load `customize`.

## Install or import

1. If the user supplied a public GitHub Skill directory URL, preserve it exactly.
2. If the user supplied only a Skill name, search the web for its public GitHub
   Skill directory. Use a result only when the repository and directory are an
   exact, high-confidence match. If results are absent or ambiguous, ask for the
   URL. Never silently choose among candidates.
3. Load the generated connector Skill `mcp-csswitch-skill-installer`.
4. Follow that connector's API and call `install_external_skill` with the exact
   `source_url`. The equivalent REPL call is:

```python
host.mcp(
    "csswitch-skill-installer",
    "install_external_skill",
    source_url=source_url,
)
```

Never call `host.skills.edit` or `host.skills.publish` as a fallback.
Never use Add Skill ZIP or `marketplace.importSkills` as a fallback.

## Uninstall, remove, or delete

1. Extract the exact Skill name from the user's current request. There is no
   default or hard-coded Skill name.
2. If no exact name is present, or more than one installed name could apply, ask
   the user. Never infer a filesystem path.
3. Load the generated connector Skill `mcp-csswitch-skill-uninstaller`.
4. Follow that connector's API and call `uninstall_external_skill` with the
   exact `skill_name`. The equivalent REPL call is:

```python
host.mcp(
    "csswitch-skill-uninstaller",
    "uninstall_external_skill",
    skill_name=skill_name,
)
```

Never call `host.skills.delete`, `skills.deleteDraft`, `host.skills.edit`, shell
commands, Python filesystem APIs, or manual filesystem deletion as a fallback.
Do not locate similarly named directories outside the CSSwitch-managed Science
data directory. If the MCP call fails, report that failure and stop.

## Result handling

Follow any native `host.agents.attach_skill` or `host.agents.detach_skill` step
explicitly returned by the connector, then perform the requested verification.
Report the response faithfully. Installation copies a directory; uninstall
moves only a CSSwitch-owned import into local quarantine. Do not claim discovery,
triggering, execution, deletion, or restart behavior that was not confirmed.
