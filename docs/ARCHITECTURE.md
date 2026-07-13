# CSSwitch architecture

This file is the current architecture contract. Release notes and dated investigations are evidence, not replacements for it.

## Product boundary

CSSwitch is a provider switcher and launcher for Claude Science. It converts a selected provider profile into the Anthropic-compatible local endpoint Science expects, manages the CSSwitch Gateway, prepares the isolated local login state, and starts or reopens Science.

Science owns its product capabilities and data: projects, organizations, native Skills, Add Skill / GitHub import, runtime resources, and upgrades. CSSwitch must not make those features startup prerequisites. In the currently verified Science build, supported external-Skill authoring/import/delete paths query the Anthropic account catalog and may fail in CSSwitch third-party mode; 0.4.4 neither emulates that catalog nor claims to fix external-Skill installation. The unreleased 0.4.5 local test build adds only a user-approved public-GitHub directory bridge: one fixed routing Skill directs natural-language requests to two scoped local MCP connectors, Science owns host-access approval and Agent-Skill `attach_skill`/`detach_skill`, and the existing CSSwitch gateway performs the bounded atomic copy or quarantines its own imports. CSSwitch attaches only that fixed route through Science's loopback one-time-nonce/CSRF UI control plane; it exposes no general control client and adds no Skill Manager, catalog, inventory, or lifecycle gate.

## Runtime flow

```text
CSSwitch provider profile
  -> CSSwitch Gateway
  -> isolated local login state
  -> persistent Science data-dir
  -> start/reuse Science
  -> open Science UI
```

The one-click path must not pass through an external Skill directory, CSSwitch Skill store, inventory, Skill catalog, reconcile, or deploy step.

## Sources of truth and ownership

| Data | Source of truth | Owner |
| --- | --- | --- |
| Provider profiles and CSSwitch settings | `~/.csswitch/` configuration | CSSwitch |
| Gateway lifecycle and local routing | CSSwitch runtime state | CSSwitch |
| Installed Science executable | `/Applications/Claude Science.app/.../claude-science` | User / Science installer |
| Persistent Science state | `~/.csswitch/sandbox/home/.claude-science` | Science |
| Version-specific runtime resources | `<data-dir>/runtime/<version>/` | Science internal implementation |
| Native and imported Skills | `<data-dir>/orgs/<active-org>/skills/<name>/` | Science organization |
| Provider capability metadata | `catalog/capabilities.v1.json` | CSSwitch |
| Legacy Skill store/inventory from 0.4.2/0.4.3 | retained but unused | Neither runtime path |

CSSwitch reuses the persistent Science data-dir across launches and Science upgrades. The normal launch path does not rebuild that directory, synchronize it in either direction, or delete user changes. The separate 0.4.5 conversation bridge may atomically ensure its exact fixed route, may copy one explicitly requested public Skill into the active organization, and may atomically quarantine only a directory carrying its own valid user-import marker. Same-name user or modified route content is never overwritten.

The App executable, persistent data-dir, version-specific runtime resources, and organization Skill directories are different facts. The data-dir provides state continuity; it is not an executable-version pin. CSSwitch normally selects the currently installed Claude Science App. A valid absolute non-symlink `SCIENCE_BIN` remains an explicit development override and fails closed when invalid.

CSSwitch never copies `bin`, `conda`, `runtime`, or `seed-assets` from the real `~/.claude-science` during initialization. Existing historical cache files are retained but are not migrated, deleted, or selected automatically. If the App is absent, a cached binary is eligible only when its version can be read and the user authorizes “use cached once” in the preflight UI. This choice exists only in memory for that launch. When the App is present it always wins.

After launch, CSSwitch records the selected binary path, source, and readable version in memory. URL, status, and stop operations use that identity. After a CSSwitch restart, recovery asks candidate binaries to confirm the same data-dir daemon; it never treats port occupancy alone as identity proof.

Science's `@` output/artifact references are request attachments, not registered Skills. They can provide file or prompt context but do not replace the Skill registry, Agent binding, scripts/resources, or persistent natural-language triggering.

## Failure boundary

Provider configuration, Gateway startup, isolated-login preparation, runtime preflight, port ownership, Science launch, and Science health/identity may fail one-click startup. A missing App without an explicitly authorized readable cache is a runtime preflight result, not a provider or Skill error. Skill counts, legacy store conflicts, inventory corruption, missing Skill catalog data, external `~/.claude/skills`, and route/MCP registration failures must not fail or restart Science.

The Skill Manager source remains recoverable from the `v0.4.3` tag and protected development worktrees, but it is not compiled, registered, packaged, or executed in the focused runtime.
