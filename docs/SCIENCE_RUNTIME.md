# Science runtime facts used by CSSwitch

Last focused verification: 2026-07-13, Claude Science `0.1.18-dev.20260709.t211149.shab3f5130-release` (`b3f5130a`). Reverify these facts when the upstream binary changes.

## Confirmed facts

- CSSwitch launches Science with a fixed, persistent data-dir: `~/.csswitch/sandbox/home/.claude-science`.
- The same directory contains active-organization state, projects, organization-owned Skills, and Science-managed version runtime resources. Restarting Science with the same directory preserves state, but the directory is not an executable-version selection policy.
- The installed `/Applications/Claude Science.app` executable is the normal runtime source of truth. `SCIENCE_BIN` remains an explicit development override; an invalid override fails closed and never falls through to another binary.
- CSSwitch no longer copies `bin`, `conda`, `runtime`, or `seed-assets` from the real `~/.claude-science` on first launch. A fresh CSSwitch data-dir can be initialized by the selected App binary even when the real directory is unreadable. Existing cache files are retained untouched.
- If the App is missing, preflight returns `cached_choice_required` only when `<data-dir>/bin/claude-science` is executable and `--version` returns a safe readable version. The user may authorize that exact cache for one launch. Unknown-version cache and missing cache return `missing`; no cache is started implicitly and no choice is persisted.
- The 2026-07-13 installed-runtime check caught a stale-selection bug: the App had already updated to `0.1.18`, while CSSwitch still launched `<data-dir>/bin/claude-science` `0.1.15`. After selecting the App, `lsof` showed the live 8990 process executing the App binary and loading the `0.1.18` runtime from the unchanged CSSwitch data-dir; `claude-science status` reported healthy on port 8990. This proves that CSSwitch had started the wrong runtime in that incident. It does **not** establish `0.1.18` as a general minimum supported Science version.
- Science's native Settings > Skills UI provides `Add skill` and `Import from GitHub`. The UI states that it accepts plugin-marketplace repositories or repositories with `skills/` directories.
- A fresh isolated Science data-dir initialized standard multi-file Skill directories under `orgs/<org-id>/skills/<skill>/`, including `SKILL.md` plus optional `scripts`, `references`, and other resources. Science displayed those Skills without CSSwitch scanning or deploying them.
- Science upgrades reuse this data-dir. Updating the Science App therefore changes the executable on the next stopped-to-started CSSwitch launch while preserving the same organizations, projects, Skills, and other data. CSSwitch must not copy the App bundle over user data or keep preferring a stale data-dir binary.
- CSSwitch records the actual launch binary path, source (`explicit`, `installed_app`, or one-shot `cached_once`), and readable version in memory. `url`, `status`, and `stop` use the same identity. After CSSwitch restarts, it only adopts a candidate whose canonical executable path matches the listener PID and whose CLI confirms the same data-dir daemon; a listening port or `status` response alone is never sufficient. The focused comparison showed why: both the cached 0.1.15 CLI and installed 0.1.18 CLI could report the same running 0.1.18 daemon.
- Organization Skills always live at `<data-dir>/orgs/<active-org>/skills/<name>/`. `<data-dir>/runtime/<version>/` is Science-managed runtime content and is never an external-Skill install target.
- The Agent-facing `host.skills` SDK exposes `list`, `read`, `edit`, `publish`, and `delete`, but no local `install` or `import` method. The UI GitHub importer uses a separate marketplace API.
- `local-mcp.json` accepts only `name`, `command`, `args`, `env`, and optional `description`; it has no supported flag for unsandboxed or trusted host execution.
- A local stdio MCP connector is Agent-discoverable as an auto-generated connector Skill, and Agent calls go through `repl` and `host.mcp`. Its child process cannot read/write the protected Science data-dir, connect to a loopback host endpoint, open a Unix socket, or create arbitrary bridge files without a Science host grant.
- Science's supported bridge is `request_host_access(mode='rw')`, followed by host-aware `edit_file` and `read_file`. The exact absolute path must be supplied by CSSwitch; the Agent must not guess a user name or path.
- Science's default `OPERON` Agent persists a restricted `skill_names` list. A directory may appear in `host.skills.list()` while `skill(name)` remains unavailable until the Skill is attached to that Agent.
- Agent-side `host.agents.attach_skill("OPERON", name)` validates an on-disk Skill, persists the binding, and refreshes the registry even while the account catalog is degraded. `detach_skill` removes that binding without using the Skill catalog.
- MCP connection and connector descriptions alone do not guarantee natural-language routing. In a real CSSwitch UI conversation, an uninstall request selected bundled `customize`, failed through catalog-gated `host.skills.delete`, then attempted filesystem deletion against a different visible `~/.claude-science` directory. A route that is merely present on disk is also insufficient; it must be attached to `OPERON`.
- The official `claude-science url --data-dir ...` command returns a one-time local URL. Science's UI uses a nonce cookie exchange plus CSRF cookie/header, and `POST /api/agents/OPERON/skills` attaches a named Skill. CSSwitch uses this flow only for the fixed `csswitch-external-skill-tools` route after health/identity checks. The bounded client accepts loopback HTTP only and does not use OAuth, a private bearer token, or direct database writes.
- In the 2026-07-13 isolated E2E, CSSwitch atomically created and control-plane-attached the fixed route. The conversation then selected `mcp-csswitch-skill-installer`, obtained host access for a dedicated bridge directory, submitted a one-time request, and the host worker atomically created the full public `internal-comms` Skill directory plus version-1 `.import-origin`; no `.catalog_stamp` was written. The bridge protocol now authenticates that request with an expiring HMAC bound to the current Gateway launch and rejects symlink/FIFO, modified, stale, or replayed request files. The Agent called native `attach_skill`, loaded the imported Skill in the current conversation, restarted Science with the same temporary data-dir, and loaded it again in a new conversation.
- In the same E2E, a later prompt containing only `请卸载 internal-comms` loaded the fixed route and the scoped `mcp-csswitch-skill-uninstaller`, called `uninstall_external_skill`, quarantined the marked directory, called native `detach_skill`, and verified that the Skill no longer loaded. It did not use `host.skills.delete`, bash, or manual filesystem deletion.
- The same E2E remained in the expected account-fetch 401 and `[skillCatalog] provider list() degraded` state. It used a temporary HOME/data-dir, dynamic ports, fake security and API material, a local mock provider, and public GitHub content; it did not use real OAuth, Keychain, SSH, or real Science organization data.
- `@` in the composer labels artifacts/outputs as artifact or attachment references backed by a version or file path. Skill mentions are a separate `type: skill` representation. An artifact supplies request context; it does not register or attach an executable/persistent Skill.

## What was not proved

The 2026-07-12 isolated GitHub preview initially stayed at `Fetching...` for both configurations below:

1. `ANTHROPIC_BASE_URL` plus process-wide `HTTPS_PROXY` through CSSwitch Gateway.
2. The same `ANTHROPIC_BASE_URL` with all process-wide proxy variables removed.

The later real-machine attempt with `https://github.com/anthropics/skills/tree/main/skills/pdf` produced an invalid GitHub API request ending in `/commits/main/skills/pdf` and HTTP 422. A conversation request to install the same Skill downloaded its files, then misrouted into the authoring flow `host.skills.edit`; Science refused the new draft because its account-backed Skill catalog was degraded.

The matching Science log showed repeated account fetch HTTP 401 responses followed by `[skillCatalog] provider list() degraded`. This proves that the currently supported UI import and Agent authoring paths are not usable without that catalog in the tested third-party session. It does not mean every standard Skill directory intrinsically requires OAuth: the later isolated bridge E2E proved that a copied directory can be attached, loaded immediately, and loaded again after restart without restoring the catalog. The installed Skill's own domain function or scripts remain a separate verification layer.

## Evidence vocabulary

Never collapse these into “installed successfully”:

1. repository/content fetched;
2. standard Skill directory created;
3. Science discovered and displayed it;
4. the Skill was selected or triggered;
5. its actual function completed;
6. the data survived a Science restart.

CSSwitch must remain fail-open with respect to all Skill stages. A future upstream verification may document reload behavior, but it must not add a startup gate.
