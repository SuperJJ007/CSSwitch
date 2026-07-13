# CSSwitch architecture

This file is the current architecture contract. Release notes and dated investigations are evidence, not replacements for it.

## Product boundary

CSSwitch is a provider switcher and launcher for Claude Science. It converts a selected provider profile into the Anthropic-compatible local endpoint Science expects, manages the CSSwitch Gateway, prepares the isolated local login state, and starts or reopens Science.

Science owns its product capabilities and data: projects, organizations, native Skills, Add Skill / GitHub import, runtime resources, and upgrades. CSSwitch must not make those features startup prerequisites. In the currently verified Science build, supported external-Skill authoring/import paths query the Anthropic account catalog and may fail in CSSwitch third-party mode; 0.4.4 neither emulates that catalog nor claims to fix external-Skill installation.

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
| Isolated Science runtime and user data | `~/.csswitch/sandbox/home/.claude-science` | Science |
| Native and imported Skills | Active organization under the Science data-dir | Science |
| Provider capability metadata | `catalog/capabilities.v1.json` | CSSwitch |
| Legacy Skill store/inventory from 0.4.2/0.4.3 | retained but unused | Neither runtime path |

CSSwitch reuses the persistent Science data-dir across launches and Science upgrades. It does not rebuild that directory, copy Skills into it, synchronize it in both directions, or delete user changes.

The executable and data directory have different ownership. For a new launch CSSwitch prefers the binary inside the locally installed official `/Applications/Claude Science.app`, while the persistent sandbox directory remains the Science-owned data source of truth. CSSwitch never reads or clones runtime assets from the user's real `~/.claude-science`. A previously retained sandbox binary remains a local fallback; CSSwitch does not download Science, invoke `claude-science update`, overwrite that fallback, or force-restart an already healthy daemon to apply version drift.

## Network exposure boundary

The CSSwitch Gateway binds loopback, and isolated Science is launched with an explicit `--host 127.0.0.1`; CSSwitch does not provide a `0.0.0.0` switch. CSSwitch assigns Science's preview listener explicitly on the port immediately after the UI port; configuration and launch preflight reject overflow, reserved, Gateway, or occupied preview ports. Raw `serve` output is discarded because the official CLI may print a data-dir or Web UI URL; CSSwitch logs only a generic result. Its remote-access helper only generates SSH client commands that forward those two ports from client loopback to server loopback. The commands use `-F /dev/null` and explicit host-key checking, so SSH config aliases and hidden forwards are not loaded. CSSwitch never forwards the Gateway port, starts an SSH process, enables macOS Remote Login, changes `sshd` or firewall configuration, or stores an SSH destination.

CSSwitch does not request or return the short-lived Science login URL. The UI exposes only secret-free commands and clears them on mode/port/runtime changes or after three minutes. The second command requests the URL over SSH after the user runs it, so the token appears only in the access-side terminal and never enters CSSwitch frontend state, configuration, clipboard-by-default, or logs.

## Failure boundary

Provider configuration, Gateway startup, isolated-login preparation, port ownership, Science launch, and Science health/identity may fail one-click startup. Skill counts, legacy store conflicts, inventory corruption, missing Skill catalog data, and external `~/.claude/skills` must not fail or restart Science.

Science version discovery is fail-open with respect to an existing healthy daemon. A missing or non-runnable official app candidate falls back to the retained sandbox binary before launch. Once a newer binary has actually attempted to open the persistent data-dir, CSSwitch must not blindly start an older binary against a potentially migrated directory.

The Skill Manager source remains recoverable from the `v0.4.3` tag and protected development worktrees, but it is not compiled, registered, packaged, or executed in the focused runtime.
