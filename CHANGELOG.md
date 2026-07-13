# Changelog

## Unreleased

### Added

- Added an opt-in SSH client helper that generates loopback-only tunnel and login commands for the isolated Science UI and its explicitly assigned preview port. CSSwitch never obtains the short-lived Science login token: it appears only in the access-side terminal after the user runs the second command. CSSwitch does not start SSH, enable Remote Login, change `sshd`, or persist the SSH target or commands.

### Changed

- New isolated Science launches prefer the binary from the locally installed official Claude Science app. The retained sandbox binary is used only when the official binary is missing or fails the non-mutating version preflight.
- Kept Science `--no-auto-update`: CSSwitch neither downloads Science nor calls its self-updater. Existing healthy daemons are reused and are never force-restarted merely because the installed app changed.

### Safety

- The CSSwitch Gateway and Science remain loopback-only by default. SSH instructions never forward the Gateway port, explicitly keep host-key checking enabled, and ignore SSH config files so hidden user-configured forwards cannot be activated by the helper.

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
- See [Upgrade and rollback](docs/upgrade-and-rollback.md).

## Previous releases

See [GitHub Releases](https://github.com/SuperJJ007/CSSwitch/releases) for notes and downloadable artifacts for v0.3.6 and earlier.
