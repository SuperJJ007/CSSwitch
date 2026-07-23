# Changelog

## [0.8.2] — 2026-07-23

### Fixed

- Made the isolated Claude Science SSH preflight consume concrete authorized Host aliases instead of relying on a cross-HOME OpenSSH Include that Science skips.
- Kept OpenCode Go `kimi-k3` consistent across saved profiles, Science selectors, `/v1/models`, and the final upstream route instead of retaining stale `kimi-k2.6` entries.
- Repaired the isolated old-to-new coverage-install acceptance so the old Gateway health phase, replacement, migrated catalog, strict routes, and cleanup complete in one run.
- Prefer a validated, content-addressed snapshot of the official updater executable for new third-party Science launches, keeping Science data isolated while avoiding the stale `/Applications` seed runtime.
- Added collision-safe, bidirectional compatibility for Claude Science's `python` tool name so Codex no longer rejects full tool-bearing conversations as a generic 502.
- Let the first Codex model-catalog request use the existing bounded refresh/retry budget instead of failing after 400 ms, so switching from official mode to third-party Codex no longer requires a second click. Static provider catalogs retain the short local timeout, and catalog failures are now reported as `catalog_verify`.

### Known limitation

- Third-party Science still runs with `--no-auto-update`; its Settings update action cannot apply an update. Update through the official Science runtime first, then stop and restart the CSSwitch-managed chain so the validated updater executable can be adopted.

## [0.8.1] — 2026-07-20

### Added

- Added two explicit OpenCode Go profiles for the official OpenAI Chat and Anthropic Messages transports, backed by a versioned official model-to-protocol route table and bare upstream model IDs.
- Added Grok (xAI) and Gemini through their official OpenAI-compatible endpoints, with scratch model discovery, manual model entry, static Science selectors, and title/classifier/tool smoke coverage.
- Added canonical Claude role aliases for Codex. Compatible aliases resolve deterministically against one authenticated account catalog snapshot while unknown aliases and raw OpenAI model IDs fail before inference.
- Added an opt-in SSH preflight bridge for isolated Science. CSSwitch writes only a private sandbox config containing an absolute `Include` of the authorized system config; it does not copy keys or expose the real `.ssh` tree.

### Changed

- Made inference POSTs exactly once, separated stream header negotiation from body forwarding, and preserved bounded/redacted upstream HTTP classifications instead of flattening all failures into 502.
- Added strict Anthropic SSE lifecycle validation. A failed or truncated stream emits one terminal error and never a synthetic `message_stop`.
- Added strict Anthropic structured-output translation for Codex Responses and fail-closed handling when a Lite model cannot preserve a forced named tool.
- Added a Sonnet-role fallback for the memory classifier without bypassing its fail-closed policy; title, summary, and status results remain plain text rather than JSON-encoded strings.

### Fixed

- Fixed OpenCode Go routes that previously reached the wrong protocol or endpoint and surfaced as “cannot connect to server.”
- Fixed K3/Kimi multi-turn recovery by preserving signed reasoning and tool semantics across turns, rejecting tampered history locally, and refusing malformed OpenAI Chat responses instead of synthesizing success.
- Fixed the stale Kimi preset to use canonical `kimi-k3`, aligned preset default/Sonnet invariants, and made profile cards show the actual Balanced/Quality/Fast/Fable routing instead of only the default model.
- Fixed Kimi Anthropic relay filtering so only zero-information thinking and complete server-tool blocks are removed. Original indexes, hidden deltas, signatures, usage, stop reason, and terminal lifecycle are validated before compact output indexes are emitted.
- Kept DeepSeek native Anthropic and DSML detect/rewrite/off behavior isolated from Kimi-specific filtering.
- Fixed isolated Science history selection across v0.8.0 sign-out and upgrade paths with a private CSSwitch marker. Ambiguous legacy multi-org state now asks the user to choose an opaque candidate instead of guessing, deleting, or rewriting history.

### Compatibility and release notes

- OpenCode Go, Grok, and Gemini support text, multi-turn conversations, tools and `tool_choice`, model discovery/manual entry, titles, and classifiers. Images, provider-specific reasoning, native streaming, and structured outputs remain explicitly limited; Gemini native API is not included.
- OpenSSH itself is covered by automated `Include` and fail-closed tests. The installed Claude Science preflight parser remains a separate real-machine acceptance step and is not claimed by those tests.
- Version 0.8.1 remains macOS Apple Silicon only. Release artifacts must continue to describe ad-hoc signing and lack of notarization unless artifact-specific evidence proves otherwise.
- Automated and loopback gates remain separate from installed-runtime, live-provider, real-account, signing, notarization, Gatekeeper, and public-release evidence.

## [0.8.0] — 2026-07-19

### Added

- Added ordered multi-model catalogs for API-key providers. A profile can use one model for every Science role or separate Quality, Balanced, Fast, and Fable models, including exact manually entered upstream IDs.
- Added strict selector-to-upstream routing for static providers. Science now sees the configured model names, while unknown selectors fail closed instead of silently falling back to a different Qwen or DeepSeek model.
- Added a real Skill & MCP page that lists the active Science organization's Skills, sources, and attachment states without exposing private paths.

### Changed

- Rebuilt the desktop UI around four focused pages—Model connections, Skill & MCP, Status, and Settings—with a 920×650.5 default window and compact right-corner runtime notifications.
- Restart Science only when the visible catalog, default model, role bindings, or runtime binding changes. Key, endpoint, or same-selector upstream changes restart only the Gateway.
- Simplified provider editing to four freely editable model fields. Discovery is optional, Codex keeps its dynamic account catalog, and the Codex edit action remains visibly disabled.
- Migrated v3 configuration atomically to schema v4 with non-overwriting backups while preserving active profiles, unknown fields, ports, and the existing Science data directory.

### Fixed

- Removed the `default` pseudo-model label from static provider catalogs and kept the selected upstream model name visible in Science.
- Made browser-open failures return a copyable fallback URL and ensured every runtime operation clears its loading state.
- Made interrupted Gateway recovery fail closed when the journal target no longer matches the active profile, leaving the listener and journal untouched for explicit recovery.

### Safety and upgrade notes

- Version 0.8.0 remains macOS Apple Silicon only, ad-hoc signed, and not notarized.
- Before rolling back to 0.7.0 or earlier, use 0.8.0's explicit export-and-downgrade flow to schema v2, or restore a compatible versioned backup after every CSSwitch process has stopped.
- Automated and loopback gates do not claim exhaustive live-provider, real-account, Developer ID, notarization, or Gatekeeper verification.

## [0.7.0] — 2026-07-17

### Added

- Added an off-by-default Codex → Claude Science bridge with a separate CSSwitch browser OAuth flow, automatic canonical Codex profile creation, and dynamic account model discovery.
- Added Responses/SSE translation for text, reasoning, tools, parallel tool calls, encrypted reasoning continuation, streaming, and non-streaming Science requests.
- Added one Codex network resolver shared by login, refresh, model discovery, scratch/formal gateways, and inference, with direct, environment, HTTP(S), SOCKS5, and SOCKS5h routes.
- Added cancellable login operations, exclusive authentication preflight, structured status/error reasons, safe logout, v3 config migration, and isolated Acceptance data roots.
- Replaced the app icon with the selected orange rounded-square switch design, including transparent corners and regenerated macOS/Windows icon assets.

### Fixed

- Accept bounded Codex SSE responses whose upstream omits `Content-Type`, while continuing to reject HTML, JSON errors, challenge pages, empty bodies, and unrelated payloads without resending inference requests.
- Prevent repeated authentication sidecars, stuck lifecycle locks, orphaned scratch processes, stale model-catalog reuse across auth generations, and configuration changes racing a successful preflight.
- Keep ad-hoc builds independent of macOS Keychain and Apple signing identities by storing only CSSwitch-owned Codex records in hardened private files.

### Safety and upgrade notes

- CSSwitch never reads, reuses, modifies, or deletes native `~/.codex` login state. Codex credentials are single-account, CSSwitch-owned records with `0700/0600` permissions, no symlinks, atomic commits, and generation/CAS checks.
- Version 0.7.0 migrates v1/v2 config to v3 with non-overwriting backups. Before rolling back to 0.6.0, use 0.7.0's “Export and downgrade to v2” flow or restore the migration-created v2 backup after all CSSwitch processes stop; deleting profiles alone does not change the schema. OAuth files are not exported or modified by downgrade.
- Codex remains experimental and off by default. Browser login is supported; device-code login, multi-account, proxy authentication, PAC, custom CAs, system-proxy discovery, and TUN detection are not.
- The release remains macOS Apple Silicon only, ad-hoc signed, and not notarized. Apple Development or Developer ID signing is not required for Codex login.

## [0.6.0] — 2026-07-16

### Added

- Added local `.zip` / `.skill` import through the desktop file picker, using the same bounded package validation, atomic commit, and native OPERON attachment as the GitHub route.
- Added bundle manifests, transaction journals, complete affected-Skill confirmation, and whole-bundle quarantine. Partial physical deletion is intentionally unsupported.
- Added single-request GitHub download progress, a fixed-commit tree/raw fallback when the archive stream cannot complete safely, terminal bridge responses, and interrupted-request recovery after a Gateway restart.

### Changed

- Repeated installation of the same verified fixed commit reuses every member without another download and reports `REUSED_VERIFIED` per Skill.
- Legacy v0.5.0 external-Skill routes migrate to the combined connector without discarding user MCP entries or unknown configuration fields.
- Bundle attachment and detachment use one OPERON batch update followed by a read-back of all affected Skills.

### Safety and upgrade notes

- GitHub installation never auto-retries or creates a replacement request after a terminal response. Request, status, and `.processing` files are cleared after success, failure, timeout, or restart recovery.
- Version 0.6.0 keeps the v2 profile format and reuses the existing isolated Science data-dir. It does not write SQLite, inventory, catalog, or the removed Skill Manager.
- The release remains Apple Silicon only, ad-hoc signed, and not notarized. Public GitHub availability and provider tool-use quality remain external dependencies.

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
