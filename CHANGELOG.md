# Changelog

## [0.5.0] — 2026-07-14

### Added

- Added a user-approved bridge for installing a complete public GitHub Skill directory from an exact URL and attaching it to Science's default Agent. The same combined local connector can quarantine and detach only CSSwitch-owned imports.
- Added an opt-in setting that lets isolated Science invoke system OpenSSH with the user's real `~/.ssh/config`. CSSwitch does not copy or link `.ssh`, start `sshd`, enable Remote Login, change the firewall, or expose a public listener.

### Changed

- New isolated Science launches prefer the binary from the locally installed official Claude Science app. A readable retained sandbox binary is offered only as a one-launch fallback when the App is unavailable and the user explicitly authorizes it; the choice is not persisted.
- Kept Science `--no-auto-update`: CSSwitch neither downloads Science nor calls its self-updater. Existing healthy daemons are reused and are never force-restarted merely because the installed app changed.
- Combined external Skill install and uninstall into one MCP process. Existing CSSwitch-managed two-connector registrations are migrated automatically; unrelated user registrations are preserved.
- Cached Science version probes by executable fingerprint and persisted successful Skill-route reconciliation state, so repeated one-click opens skip redundant CLI and control-plane work until the runtime or registration changes.
- Hardened DeepSeek DSML tool-call normalization for third-party Science conversations.

### Safety

- The CSSwitch Gateway and Science remain bound to loopback. Version 0.5.0 does not add a `0.0.0.0` switch or a public-network entry point.
- System SSH reuse is off by default and is an explicit trust grant: normal OpenSSH `Include`, `IdentityFile`, `IdentityAgent`, `ProxyCommand`, and `Match exec` behavior may apply when enabled.
- Skill installation still requires Science's host-access approval, exact public source URL, bounded authenticated requests, and native Agent attach/detach. It does not emulate OAuth/catalog access, overwrite existing Skills, or directly edit Science databases.
- Explicit quit stops the managed Science daemon before the Gateway; merely closing the settings window keeps the local chain running.

### Upgrade notes

- Version 0.5.0 keeps the v2 configuration schema and reuses `~/.csswitch/sandbox/home/.claude-science`; existing organizations, projects, Skills, and legacy Skill Manager files are retained.
- The release remains Apple Silicon only, ad-hoc signed, and not notarized. Name-only Skill source discovery is provider-dependent; private repositories, updates/overwrite, and permanent-delete/restore UI are not included.

## [0.4.4] — 2026-07-12

### Changed

- Removed Skill Manager from the compiled application, Tauri command registry, and one-click startup path. Science remains the owner of its persistent data-dir and native Skill lifecycle.
- One-click startup no longer scans external or workspace Skills, reads or recovers the legacy store/inventory, reconciles deployments, stops Science for Skill changes, or requires a reconcile marker.

### Compatibility

- Continue to reuse `~/.csswitch/sandbox/home/.claude-science`; existing Science organization, project, and Skill data is left untouched.
- Keep legacy CSSwitch Skill store/inventory files in place but unused. Large or unreadable external Skill trees, `STORE_CONFLICT`, broken inventory, and missing Skill catalog data cannot block startup.
- Science's supported external-Skill authoring and GitHub-import paths may require a valid Anthropic account catalog. Version 0.4.4 does not bypass OAuth, emulate that catalog, or claim that natural-language external-Skill installation works in third-party mode.

## [0.4.3] — 2026-07-12

### Fixed

- Import a single-file Skill created by a Science agent as `<name>.skill.md` in the active workspace root, then persist it in the CSSwitch store and deploy it through the existing serialized Science lifecycle.
- Automatically restart isolated Science when a managed Skill changes, so one click completes import, deployment, and activation without a separate manual stop/start cycle.
- Recover from `STORE_CONFLICT` without deleting evidence: quarantine the complete old Skill root, re-inspect and restore valid payloads, preserve skipped content in the quarantine, and retry startup once.

### Safety

- Workspace ingress is limited to direct `*.skill.md` files under the trusted active organization, rejects symlinks and hardlinks, caps size/count, verifies a stable file identity, and never reads credentials or arbitrary HOME paths.
- Product wording now consistently describes CSSwitch as a configuration converter that connects Science to the user's own API.

CSSwitch follows semantic versioning. Older release notes remain available on the [GitHub Releases page](https://github.com/SuperJJ007/CSSwitch/releases).

## [0.4.2] — 2026-07-12

### Added

- Added a local Skill Manager that discovers compatible Skills from configured real-HOME sources, imports immutable copies into CSSwitch-managed storage, tracks inventory, and deploys selected Skills into the isolated Science sandbox.
- Added requirement inspection and compatibility reporting for Skill files, scripts, assets, references, static trees, and external Python, R, network, or tool dependencies.
- Expanded the capability catalog and installation-matrix checks used by runtime diagnostics.

### Safety and lifecycle

- Imported Skills remain in managed storage if their original source disappears, and sandbox reconstruction restores deployed Skills from that store.
- Skill discovery and deployment do not read or copy real Science credentials. The existing sandbox lifecycle and local-only runtime boundaries remain in force.
- Preserved 0.4.1's exact legacy Python proxy ownership checks: cleanup still requires the expected listener port, UID, Python identity, bundled legacy script path, and provider arguments; unknown processes fail closed.

### Upgrade notes

- Replace the existing app with `CSSwitch_0.4.2_aarch64.dmg`; v2 profiles remain compatible.
- Existing imported Skills are retained under CSSwitch-managed data. Back up `~/.csswitch/` before upgrading or rolling back.
- This release remains Apple Silicon only, ad-hoc signed, and not notarized.

## [0.4.1] — 2026-07-11

### Fixed

- Fixed upgrades from Python-based releases leaving an orphaned CSSwitch proxy on the configured port and blocking “Start.”
- CSSwitch now stops a legacy listener only when the listening PID, current user, Python process name, exact previous bundle script path, provider argument, and configured port all match.
- Unknown listeners, unrelated Python processes, and unverified stale gateways remain untouched and continue to fail closed.

### Upgrade notes

- Replace the existing app with `CSSwitch_0.4.1_aarch64.dmg`; v2 profiles remain compatible.
- The first start may take a moment while an exact legacy CSSwitch Python proxy exits and the Rust gateway takes ownership of the port.
- If CSSwitch cannot prove that a listener belongs to the legacy bundle, it will still ask you to choose a free port or stop that process manually.

## [0.4.0] — 2026-07-11

### Added

- A bundled Rust inference gateway for DeepSeek, Qwen, Anthropic-compatible relays, custom OpenAI Chat Completions, and OpenAI Responses.
- Stronger gateway health identity using provider, compatibility mode, and launch identity.
- Broader provider compatibility coverage for model mapping, tool calls, streaming, retries, and error handling.

### Changed

- Production inference, profile validation, and model discovery now use the bundled Rust gateway.
- The production app no longer ships a Python inference proxy or Python fallback.
- Provider compatibility behavior is centralized in the Rust gateway and capability catalog.
- Configuration schema remains v2, so normal in-place upgrades preserve existing profiles.

### Fixed

- Reduced the chance of accepting an unrelated or stale local listener as the active gateway.
- Improved owned-process cleanup during failed startup, stop, and application exit.
- Aligned scratch validation, model discovery, activation, and status reporting with the active profile’s adapter.

### Upgrade notes

- Download `CSSwitch_0.4.0_aarch64.dmg` and replace the existing app in Applications.
- Back up `~/.csswitch/config.json` before upgrading.
- Rollback requires reinstalling the previous stable app; there is no runtime Python-backend switch.
- The macOS build is Apple Silicon only, ad-hoc signed, and not notarized. First launch may require right-clicking the app and choosing “Open.”
- See [Upgrade and rollback](docs/operations/upgrade-and-rollback.md).

## Previous releases

See [GitHub Releases](https://github.com/SuperJJ007/CSSwitch/releases) for notes and downloadable artifacts for v0.3.6 and earlier.
