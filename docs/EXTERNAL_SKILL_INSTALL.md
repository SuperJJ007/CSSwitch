# External Skill install bridge (0.4.5 local test build)

This is a narrow bridge for a missing third-party-mode workflow. It is not a Skill Manager and is not part of the published 0.4.4 release.

## User behavior

The primary Science request is:

```text
请安装这个外部 Skill：
https://github.com/owner/repo/tree/ref/path
```

Science first loads the attached `csswitch-external-skill-tools` routing Skill. It directs the Agent to the generated `mcp-csswitch-skill-installer` connector Skill, whose only tool is `install_external_skill`. After the normal MCP confirmation, the Agent requests read-write access to a dedicated `~/CSSwitch-Skill-Bridge-*` directory, submits a one-time request, and reads the response.

File copy is not reported as a usable installation. A successful copy returns `FILES_COMMITTED_ATTACH_REQUIRED` with the exact Skill name. The Agent must then call:

```python
host.agents.attach_skill("OPERON", skill_name)
```

and load `skill_name` with `skill()` before reporting it usable. This uses Science's native Agent-Skill binding and registry refresh rather than writing the Science database or calling a private HTTP endpoint.

If the user supplies only a name, the MCP tool returns `NEED_SOURCE_URL` without writing files. The Agent may use its normal search capability to find candidate public repositories, but it must not install an ambiguous guess. The bridge itself never searches for or guesses a repository and always requires an exact URL.

For removal, the routing Skill directs Science to the separate `mcp-csswitch-skill-uninstaller` connector. Its only tool is `uninstall_external_skill`. It accepts an exact Skill name only when the directory carries a valid CSSwitch `.import-origin`, moves the complete directory to `~/.csswitch/sandbox/skill-trash/`, and returns `QUARANTINED_DETACH_REQUIRED`. The Agent must then call:

```python
host.agents.detach_skill("OPERON", skill_name)
```

and verify that `skill()` no longer loads it. It must not fall back to catalog-gated `host.skills.delete` or `skills.deleteDraft`.

## Why a routing Skill is required

MCP connection alone did not make natural-language selection deterministic. A real UI conversation asking to remove an imported Skill repeatedly selected bundled `customize`, called catalog-gated `host.skills.delete`, then attempted manual deletion in the wrong visible `~/.claude-science` directory. Connector descriptions and auto-attachment were insufficient.

The current design therefore installs one tiny standard Skill, `csswitch-external-skill-tools`, and attaches it to the default `OPERON` Agent. It contains routing instructions only: install and uninstall must use the matching scoped connector, never `customize`, `host.skills.*`, shell deletion, or filesystem probing. The route is written atomically with a `csswitch-system-bridge` ownership marker. Existing same-name user or modified content is never overwritten. The route itself is not removable through the external-Skill uninstaller because its marker is not a user import.

After Science is healthy, CSSwitch obtains a dedicated one-time URL from the official `claude-science url --data-dir ...` command and uses the same local nonce/cookie/CSRF flow as the Science UI to attach this fixed route to `OPERON`. The gateway subcommand accepts only loopback HTTP, a valid one-time nonce, the fixed Agent, and the fixed route name. The nonce is passed through the child environment rather than argv, and a fresh one-time URL is generated for the browser afterward. There is no OAuth emulation, private bearer token, or database write. This bounded UI control-plane contract is an upstream compatibility risk and is covered by focused tests.

## Data flow and trust boundary

1. CSSwitch starts its normal authenticated local gateway and creates a mode-0700 bridge directory derived from the existing persistent CSSwitch secret. A separate mode-0600 bridge signing key is derived from both that secret and the per-process Gateway launch identity, so restarting Gateway invalidates requests signed for the previous process.
2. Before Science starts, CSSwitch atomically merges two managed stdio entries into `<data-dir>/mcp/local-mcp.json`: install-only and uninstall-only. Their environment contains only the path to the private key file, never the key value. CSSwitch also atomically ensures the fixed route Skill. Existing unrelated entries and unknown fields are preserved; malformed, same-name unmanaged, or modified route content fails soft.
3. Science exposes each connector as an auto-generated connector Skill. MCP remains sandboxed and does not receive direct access to the Science organization directory.
4. After health and identity checks, CSSwitch uses a dedicated one-time local Science URL to attach the fixed route Skill to `OPERON`; failure only degrades the external-Skill feature and never fails Science startup.
5. A matching natural-language request loads the route, then the scoped connector. The user grants the dedicated bridge directory through Science's `request_host_access` flow.
6. For install, `edit_file` submits a bounded, HMAC-signed request containing a random ID and short expiry. The host accepts only a same-owner, non-symlink regular request file, rejects stale, modified, mismatched, or replayed requests, then re-reads `active-org.json`, resolves the GitHub ref/path, downloads the complete directory, validates limits and paths, writes a version-1 `.import-origin`, and atomically renames it into `orgs/<active-org>/skills/<skill-name>` without overwriting an existing directory.
7. The Agent calls Science's native `host.agents.attach_skill`, then verifies the `skill()` result.
8. For uninstall, the gateway validates the exact name and CSSwitch marker, then atomically moves the directory into quarantine. The Agent calls native `detach_skill`, then verifies it is no longer loadable.

The connector does not bypass Science's sandbox. Direct MCP writes, MCP loopback access, Unix sockets, private bearer tokens, and direct database writes are not used. The separate startup-only route attachment uses Science's own loopback nonce/CSRF control plane with a fixed action.

## Confirmed facts

Confirmed by focused tests:

- separate install-only and uninstall-only MCP schemas and registration;
- Chinese and English discovery descriptions;
- name-only `NEED_SOURCE_URL` with no file write;
- public GitHub URL/ref/path resolution and complete multi-file download;
- traversal, symlink, redirect, file-count, per-file, total-size, duplicate-target, and atomic no-replace protections;
- HMAC authentication, launch-bound key rotation, request expiry/replay rejection, and nonblocking rejection of symlink/FIFO bridge entries;
- persistent per-Skill advisory locks that recover after a crash while still serializing concurrent install/uninstall requests;
- Science-compatible version-1 `.import-origin` with CSSwitch-specific ownership and no `.catalog_stamp`;
- quarantine uninstall, repeat-uninstall failure, and refusal to remove bundled, unmarked, hand-written, or foreign imported directories;
- atomic route creation, idempotence, fixed ownership marker, and preservation of modified or user-owned same-name content;
- fixed loopback-only nonce/CSRF route attachment with no nonce in argv and a fresh browser URL;
- every registration or Skill failure remains non-fatal to one-click Science startup.
- install, attach, load, restart, uninstall, and detach leave every `<data-dir>/runtime/<version>/` directory outside the write target; external Skill files are committed only under the active organization.

Confirmed by the 2026-07-13 isolated real-Science E2E using a temporary HOME/data-dir, dynamic ports, fake security/key material, a local mock provider, and public `anthropics/skills/internal-comms`:

- Science ran with account fetch 401 and `skillCatalog` degraded;
- the conversation discovered and loaded `mcp-csswitch-skill-installer`;
- the Agent used `host.mcp`, `request_host_access`, `edit_file`, and `read_file`, not `host.skills.edit/publish`;
- content was fetched and the complete directory plus `.import-origin` was atomically committed;
- `host.agents.attach_skill("OPERON", "internal-comms")` persisted the binding;
- `skill("internal-comms")` returned imported Skill metadata and complete instructions in the current conversation;
- after stopping and restarting Science with the same temporary data-dir, a new conversation loaded the same imported Skill again;
- a later conversation containing only `请卸载 internal-comms` discovered and loaded `csswitch-external-skill-tools`, then loaded `mcp-csswitch-skill-uninstaller`;
- the Agent called `uninstall_external_skill`, the host quarantined the marked directory, native `detach_skill` removed the binding, and `skill("internal-comms")` no longer loaded;
- the natural-language uninstall round did not call `host.skills.delete`, `skills.deleteDraft`, bash, or manual filesystem deletion;
- no real OAuth token, API key, Keychain credential, SSH material, or real Science organization data was used.

Still separate or awaiting UI verification:

- final manual UI confirmation in the locally installed CSSwitch app after confirming that it launches the currently installed Science executable rather than a stale data-dir fallback;
- a particular installed Skill's own scripts/assets and domain function execution;
- provider-dependent behavior when an Agent searches from a name-only request;
- recovery/restore UI for quarantined content.

## `@` artifacts/outputs are not Skills

Science renders `@` output references as artifact or attachment objects backed by an artifact version or file path. They add data to the current request. Skill mentions are represented separately as `type: skill` and go through the Skill registry.

An artifact can carry a prompt document and may imitate a prompt-only Skill when the user explicitly attaches it. It does not register a Skill, attach it to `OPERON`, automatically load scripts/references, persist natural-language triggering, or bypass the catalog gate of Add Skill/ZIP import. It is useful for context reuse, not an alternative external-Skill installer.

## Focused tests

- Gateway Rust tests: URL parsing, ref resolution, download limits, path/symlink rejection, signed-request validation, FIFO rejection, stale-lock recovery, marker validation, atomic install/quarantine, MCP schemas, and result vocabulary.
- Desktop Rust tests: connector merge/migration, fail-soft registration, atomic route creation, unmanaged-content preservation, and the ignored real-Science E2E.
- Python contract tests: MCP all-mode compatibility, scoped uninstaller exposure, host-access protocol, unsigned/replay rejection through a dynamically bound Gateway, prelaunch registration, route attachment, dynamic uninstall naming, and native attach/detach result contract.
- Real E2E: `runtime::skill_install_bridge::real_science_e2e::isolated_science_installs_attaches_and_persists_external_skill`.

## Fail-safe and compatibility

- Installer registration and every Skill failure are warning-only for CSSwitch startup.
- A running Science instance is inspected read-only; changed MCP configuration requires a Science restart.
- The fixed route is re-attached idempotently on one-click open when registration is already current. Route attachment failure remains warning-only.
- Host-access denial is final for that request. The Agent must report it, not retry alternate paths or fall back to authoring/catalog APIs.
- `.import-origin`, local MCP loading, the local nonce/CSRF Agent-Skill endpoint, and `host.agents.attach_skill/detach_skill` behavior are observed Science contracts. Re-run the focused E2E after every bundled Science upgrade.
- CSSwitch does not pin a Science version in the persistent data-dir. Normal selection is a valid explicit development `SCIENCE_BIN` override or the currently installed Science App. If the App is missing, a readable cache requires explicit one-launch UI authorization and is never persisted as a preference.
- The local 0.1.15/0.1.18 observation was a wrong-binary startup incident, not evidence for a universal minimum version. Each actually selected Science version is probed and receives an idempotent route/MCP registration attempt. Failure is a `WARNING` for the external-Skill feature and never blocks Gateway or Science startup.
- Install and uninstall resolve `active-org.json` and operate only on `<data-dir>/orgs/<active-org>/skills/<name>`. They never write to `<data-dir>/runtime/<version>/skills` or any version-runtime directory.
- Local uninstall never calls Science catalog APIs and retains the complete directory in quarantine for manual recovery.

## Explicitly out of scope

No OAuth or catalog emulation; no Science binary patch; no private Science bearer use; no direct database writes; no bundled `customize` modification; no general Science control client; no Skill Manager, store, inventory, catalog, deployer, sync, general backup, update/overwrite, permanent deletion, restore UI, version manager, private-repository credentials, Python/R environment management, or domain libraries.

## Accurate 0.4.5 wording

“CSSwitch 0.4.5 local test build inherits the user's currently installed Claude Science App and adds a fixed routing Skill plus scoped local install and uninstall connectors. In an isolated real-Science UI test with the Anthropic catalog degraded, a URL request installed a complete public GitHub Skill into the active organization through a user-approved bridge, attached and loaded it immediately, and loaded it again after restart. A later name-only uninstall request selected the CSSwitch route, quarantined only the CSSwitch-owned import, detached it, and verified it no longer loaded without catalog-gated delete or shell removal. App-missing cache use requires explicit one-launch authorization. Name-only web source selection remains provider-dependent; private repositories, updates, overwrite, permanent deletion/restore UI, and each Skill's own domain function execution are not claimed.”
