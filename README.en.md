<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.7.0"><img src="https://img.shields.io/badge/release-v0.7.0-2ea44f.svg" alt="CSSwitch v0.7.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-1d1d1f.svg" alt="macOS Apple Silicon">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
</p>

<p align="center">
  <a href="./README.md">简体中文</a> ·
  <a href="./README.en.md">English</a>
</p>

# CSSwitch

CSSwitch is a local configuration converter for Claude Science. It translates Science inference requests and connects them to your own model API, including DeepSeek, Qwen, Kimi, MiniMax, GLM, OpenRouter, relay providers, or custom compatible endpoints. It can also connect the Codex models available to your account through a separate CSSwitch browser login.

It is built for more than developers. Install Claude Science, choose either a third-party API key or Codex login, make the profile active, then click "一键开始" (Start).

> The current app mainly targets macOS Apple Silicon. Because the app is not notarized yet, macOS may ask you to right-click and choose "Open" the first time.

[Download latest release](https://github.com/SuperJJ007/CSSwitch/releases/latest) · [Documentation](./docs/README.md) · [Changelog](./CHANGELOG.md) · [Report a bug](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [Request a feature](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

> **0.7.0:** Adds an off-by-default experimental Codex bridge. CSSwitch performs its own browser OAuth, dynamically reads the account model catalog, and converts Codex Responses/SSE for Science through the Rust Gateway. Login, model discovery, inference, and logout share one network route; native Codex login is never read or modified. The ad-hoc release does not require an Apple developer account. This release also adopts the new orange rounded-square icon and fixes Codex SSE responses without `Content-Type` being misreported as temporary Claude unavailability. See the [Codex → Claude Science contract](./docs/features/codex-science-bridge.md).

## Contents

- [Why CSSwitch exists](#why-csswitch-exists)
- [What it can do](#what-it-can-do)
- [Quick start](#quick-start)
- [Using Codex](#using-codex)
- [Installing and removing external Skills](#installing-and-removing-external-skills)
- [Upgrading from an older version](#upgrading-from-an-older-version)
- [Supported model sources](#supported-model-sources)
- [Status diagnostics and capability catalog](#status-diagnostics-and-capability-catalog)
- [How it protects your real account](#how-it-protects-your-real-account)
- [Current limitations](#current-limitations)
- [Languages](#languages)
- [Development](#development)
- [Risk and disclaimer](#risk-and-disclaimer)

## Why CSSwitch exists

Claude Science is Anthropic's AI agent app for research and analysis workflows, including literature review, data processing, code execution, chart generation, and writing. By default, it depends on Claude login and Anthropic inference.

CSSwitch acts as a local runtime control plane:

- It starts Claude Science in an isolated sandbox.
- It runs third-party model mode in a separate local workspace without taking over your real Claude account.
- It forwards Science model requests to the provider you choose.
- It translates between Anthropic Messages API and OpenAI-compatible APIs when needed.
- It keeps an "Official Claude" mode so subscribers can switch back to the real Science app.

In short: CSSwitch is to Claude Science what CC Switch is to Claude Code, with additional desktop-app, isolated-workspace, and local-gateway management.

```text
Claude Science sandbox
  -> CSSwitch local proxy
  -> DeepSeek / Qwen / Kimi / MiniMax / GLM / OpenRouter / Codex / custom endpoint
```

## What it can do

**For everyday users**

- Manage multiple model profiles from a desktop panel instead of editing environment variables.
- Save multiple profiles for the same provider, such as different keys, models, or relay URLs.
- Verify a key before making a profile active; failed checks do not silently switch your active setup.
- Click "一键开始" (Start) to launch the proxy, prepare the sandbox, and open Science.
- Show the actual selected model name in Science instead of a vague `claude` or `opus` label.
- Complete a separate CSSwitch Codex browser login and choose among the account's dynamic `Codex / …` entries under Science “More models.”
- Switch back to "Official Claude" without interfering with your real Claude login.
- Reuse Science's persistent data-dir; Skill state is not a CSSwitch startup gate. A Skill or bundle can be installed from an exact public GitHub URL or local `.zip` / `.skill` file; bundle removal requires whole-bundle confirmation and quarantines only CSSwitch-owned imports.
- CSSwitch inherits the Science version currently installed in `/Applications/Claude Science.app`; it does not compare, pin, upgrade, or downgrade that version. After the App updates, the next launch uses the updated App executable with the same persistent data-dir.
- If the Science App is missing, CSSwitch never starts a data-dir cache silently. Only an executable cache with a readable version can be authorized for this launch once; the choice is not saved. Otherwise the UI offers the [official Claude download page](https://claude.com/download) or cancel.

**For advanced users**

- Supports native Anthropic-compatible endpoints, OpenAI Chat Completions-compatible endpoints, and OpenAI Responses-compatible endpoints.
- Supports custom `base_url`, model names, and relay providers.
- Native Anthropic endpoints such as DeepSeek, Kimi, and MiniMax are passed through when possible to preserve tool use, thinking, and streaming behavior.
- Qwen and custom OpenAI endpoints are translated by the local proxy.
- Local config and logs make debugging and issue reports easier.

## Quick start

Before starting, make sure you have:

- [Claude Science (official Claude download page)](https://claude.com/download)
- A macOS Apple Silicon device
- A working third-party model API key, or a Codex account that can complete browser authorization
- No separate Python runtime is required; CSSwitch bundles its Rust inference gateway

1. Download the latest `CSSwitch_*.dmg` from [GitHub Releases](https://github.com/SuperJJ007/CSSwitch/releases/latest).
2. Drag CSSwitch into Applications.
3. If macOS blocks the first launch, right-click the app and choose "Open".
4. Keep the top mode set to "第三方模型" (third-party model).
5. Click "+ 新建" (New), choose a provider, and enter your API key, model, and `base_url` when required.
6. Click "创建" (Create), then choose "设为当前" (Set active) on the profile.
7. After verification succeeds, click "一键开始" (Start).
8. CSSwitch starts the isolated Science instance and opens it in your browser.

CSSwitch does not choose a Science version for you. Normal startup uses the currently installed Claude Science App. If the App is missing, the panel shows an exact readable cache version and asks whether to use it for this launch only, or to open the official download page. That choice is not persisted, and a later detected App automatically becomes the default again.

To use Science with its official service configuration, switch to "官方 Claude" (Official Claude). CSSwitch will tear down the third-party proxy path and open the real Science app.

## Using Codex

Codex remains an explicitly enabled experimental capability in 0.7.0. It uses CSSwitch-owned browser OAuth and private authentication files, never reads, reuses, or modifies native `~/.codex` login state, and does not require Apple Development or Developer ID signing.

1. Open Advanced Settings and enable the Codex experiment.
2. Choose the Codex network route. `auto` uses available `HTTPS_PROXY` / `ALL_PROXY` and `NO_PROXY` variables. An app launched from Finder usually does not inherit terminal proxy variables, so it may show `direct`; a system TUN can still take over that direct socket. You can instead enter an unauthenticated HTTP, HTTPS, SOCKS5, or SOCKS5h proxy.
3. Click Codex browser login and complete account verification and authorization on the OpenAI page. Return to CSSwitch after the browser reaches `localhost` and reports completion.
4. A successful login ensures that a Codex profile exists but never changes the active provider automatically. Make that profile active, then click "一键开始" (Start).
5. Choose a `Codex / …` entry from Science “More models.” The list comes from the current account catalog rather than a model list hard-coded by CSSwitch.

The current boundary is macOS Apple Silicon, one CSSwitch Codex account, and browser login. Multi-account, device-code login, proxy authentication, PAC, custom CAs, system-proxy discovery, and TUN detection are not supported. The system browser uses its own network configuration; the CSSwitch Codex route controls Gateway HTTPS traffic such as token exchange, model discovery, and inference.

## Installing and removing external Skills

CSSwitch provides GitHub and local-file installation paths. Both use the same path, size, symlink, and reserved-file validation, then attach the installed Skills to Science's default Agent through the native control path. The installer does not sign in to GitHub for you or read or take over Science credentials.

**Install from a public GitHub URL**

1. Complete "一键开始" (Start) in CSSwitch, then open a new Science conversation.
2. Send the Agent an exact public GitHub tree URL and explicitly ask it to use the CSSwitch external Skill installer. For example:

   ```text
   Use the CSSwitch external Skill installer for this fixed commit. Create only one request and do not retry automatically:
   https://github.com/<owner>/<repo>/tree/<commit>/<path>
   ```

3. When Science asks for access to the CSSwitch bridge directory, verify the path and approve read/write access for this request.
4. Let the same request continue while the download progresses; do not submit the install instruction again. On success, a single Skill asks for a current-conversation load check, while a bundle reports its complete member count and attachment result.

A fixed commit URL is recommended for reproducible verification and fast reuse. A public repository-root bundle can use `.../tree/<commit>`. Private repositories, name search, overwrite updates, and Skill publishing are outside the current scope.

**Install from a local file**

1. Keep isolated Science running and healthy.
2. Click "导入 Skill 包" (Import Skill package) in the CSSwitch panel.
3. Select a `.zip` or `.skill` file. Supported layouts include a root-level `SKILL.md`, one outer Skill directory, or a bundle with multiple direct Skill children and an optional `_shared` support directory.
4. After CSSwitch validates, atomically commits, and attaches the package, load the Skill in a new Science conversation to confirm it can actually run.

Installing the exact same fixed GitHub commit or local archive again verifies and reuses the existing content without another download or duplicate Skill. A package with the same Skill name but different content or provenance is rejected instead of overwriting the installed copy.

**Removal and bundle confirmation**

Ask the CSSwitch uninstaller to remove a Skill from Science. A single-Skill removal only handles a directory carrying a valid CSSwitch import marker, quarantines it, and completes Agent detachment. If the target belongs to a bundle, the first call only returns the bundle name, complete affected-Skill list, and confirmation ID; no files are removed until the user explicitly confirms whole-bundle removal. Cancellation must not invoke the uninstall tool again, and this version does not physically delete only one bundle member.

See the [external Skill bridge](./docs/features/external-skill-bridge.md) for detailed statuses, limits, recovery behavior, and troubleshooting.

## Upgrading from an older version

Version 0.7.0 atomically migrates v1/v2 configuration to v3 and preserves non-overwriting versioned backups before commit. Existing API providers, the active profile, ports, unknown fields, and `~/.csswitch/sandbox/home/.claude-science` remain intact. Config v3 adds Codex profiles and network settings. Before rolling back to 0.6.0, use “Export and downgrade to v2” in 0.7.0, or stop every CSSwitch process and restore the migration-created v2 backup; merely deleting Codex profiles does not change the schema. Legacy Skill store/inventory files remain untouched but do not participate in startup, and external `~/.claude/skills` trees are not synchronized into Science.

For exact steps, backup locations, and rollback boundaries, see [Upgrade and rollback](./docs/operations/upgrade-and-rollback.md).

## Supported model sources

| Source | API path | Notes |
|---|---|---|
| DeepSeek | Native Anthropic endpoint | Default source; preserves thinking, tool use, and streaming as much as possible |
| Qwen | OpenAI Chat Completions-compatible endpoint | CSSwitch translates it into Anthropic format for Science |
| GLM | Anthropic-compatible endpoint | Editable default URL; choose or type a model |
| Xiaomi MiMo | Anthropic-compatible endpoint | Can be changed to plan-specific or regional endpoints |
| SiliconFlow | Anthropic-compatible endpoint | Choose or type a model |
| Kimi / Moonshot | Anthropic-compatible endpoint | Editable default URL; supports Kimi models |
| MiniMax | Anthropic-compatible endpoint | Editable default URL; supports MiniMax models |
| OpenRouter | Anthropic-compatible aggregation endpoint | Choose or type a model |
| Custom Anthropic | User-provided compatible endpoint | For private gateways, Claude-compatible relays, or local adapters |
| Custom OpenAI | User-provided OpenAI Chat Completions base root | The proxy appends `/chat/completions` and `/models` |
| Custom OpenAI Responses | User-provided OpenAI Responses base root | The proxy appends `/responses` and `/models` |
| Codex (experiment, off by default) | Separate CSSwitch OAuth + Responses/SSE | Dynamically reads the current account catalog; never reuses native Codex login; single account and browser login |

> If your URL is an `/anthropic` endpoint, choose "Custom Anthropic". If you choose "Custom OpenAI", enter an OpenAI-compatible base root such as `https://example.com/v1`, not an Anthropic endpoint.

## Status diagnostics and capability catalog

CSSwitch includes a read-only capability catalog for known provider, tool-use, and transport compatibility boundaries. Runtime diagnostics return rules matched by the current profile to explain how that configuration is handled.

This catalog is for diagnostics and observability. It does not mean every external provider, official hosted capability, signing state, or notarization state has been verified.

Status lights are local observations only. For example, the sandbox light is local HTTP health, not proof that the port has been identity-verified as the CSSwitch sandbox Science instance. `Doctor` skips the real `~/.claude-science` path by default; checking whether the real HOME path exists requires explicitly setting `CSSWITCH_DOCTOR_CHECK_REAL_HOME=1`.

## How it protects your real account

CSSwitch's core boundary is simple: third-party model mode keeps credentials, data directories, and proxy routing inside the sandbox. It does not take over your real Claude account.

- It does not copy, read, or modify real Claude login credentials, OAuth tokens, account state, or user data.
- The isolated Science instance uses its own HOME, ports, and data directory.
- Third-party API keys are stored in `~/.csswitch/config.json` with `0600` file permissions.
- Experimental Codex credentials stay in private files under the CSSwitch data root: parent directories are `0700`, auth files are `0600`, symlinks are rejected, and commits use atomic generation/CAS rules. CSSwitch does not use macOS Keychain and does not read, copy, overwrite, or delete native `~/.codex` login state.
- Keys are not displayed in application logs, and the local gateway listens only on loopback.
- “Allow isolated Science to use system SSH configuration” is off by default. When enabled, it only makes real `~/.ssh/config` available to system OpenSSH calls made by Science. CSSwitch does not copy or link all of `.ssh`, start `sshd`, change the firewall, or expose a `0.0.0.0` listener. Native OpenSSH `Include`, key, Agent, and command rules still apply, so this is an explicit trust grant.
- New isolated Science launches prefer the binary from the locally installed official Claude Science app. If the App is unavailable, a readable retained sandbox copy is used only after explicit one-launch authorization; the choice is not persisted. CSSwitch does not download Science and keeps `--no-auto-update`.
- Official Claude mode tears down the third-party proxy path before handing you back to the real Science app.

## Current limitations

CSSwitch is not an official Claude service, and third-party model mode does not grant Anthropic account privileges. These are current architectural limits:

- Anthropic-hosted remote MCP services are unavailable, including `pubmed`, `clinical-trials`, `chembl`, `biorxiv`, and other `*.mcp.claude.com` services.
- Directory connectors, remote plugins, and cloud features that require real Claude account authorization may show session expired, unavailable, or skipped.
- Science's native GitHub import, Skill publishing, and draft deletion may still query the Anthropic account catalog. CSSwitch does not emulate OAuth or that catalog; the external Skill bridge supports only exact public GitHub URLs or a user-selected local `.zip` / `.skill` file and quarantines CSSwitch-owned bundles, not deterministic name search, overwrite, member-level physical deletion, permanent-delete/restore UI, private repositories, or publishing.
- Third-party models differ in tool use, long context, thinking, image, and streaming compatibility. Native Anthropic endpoints are usually more reliable than OpenAI translation paths.
- The macOS package is not notarized yet, so the first launch requires manual approval.
- The inference gateway is a bundled Rust sidecar; no runtime Python fallback is shipped.
- Codex remains an off-by-default experiment that depends on current OpenAI account entitlements, model catalog, and upstream protocol. Its single-account and network boundaries are not an official support or stability guarantee.

Please report problems through [GitHub Issues](https://github.com/SuperJJ007/CSSwitch/issues).

## Languages

README languages currently available:

| Language | File |
|---|---|
| Simplified Chinese | [README.md](./README.md) |
| English | [README.en.md](./README.en.md) |

The desktop app UI is currently mainly Chinese. Multilingual README files do not mean the app UI already has an in-app language switch. If app i18n lands later, this section will say so explicitly.

## Feedback and community

When reporting a problem, please include:

- CSSwitch version
- macOS version and chip architecture
- Provider and model
- Steps to reproduce
- Relevant logs from `~/.csswitch/logs/`

Please remove API keys, tokens, email addresses, private URLs, and any sensitive data before submitting logs.

- [Report a bug](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml)
- [Request a feature](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)
- [Read the changelog](./CHANGELOG.md)

## Development

Users do not need to run CSSwitch from source. This section is for debugging and contributors.

Maintainers can use the [documentation index](./docs/README.md) for architecture, testing, release operations, feature contracts, and versioned evidence.

```bash
cd desktop
npm install
npm run tauri dev
```

Common checks:

```bash
bash test/run_all.sh
bash test/run_all.sh --require-release-ready

(cd desktop/gateway && cargo test)
(cd desktop/src-tauri && cargo test)
python3 -m unittest discover -s test -p 'test_*.py' -v
node --check desktop/src/main.js
```

## Risk and disclaimer

- This project is for personal learning and research. Use it at your own risk.
- CSSwitch is not affiliated with, endorsed by, or partnered with Anthropic.
- Inference requests are sent to the third-party model service you configure and pay for.
- Third-party model mode does not grant official Anthropic account permissions; some official hosted capabilities may remain unavailable.
- The software is provided "as is", without warranty of any kind.

## Acknowledgements

CSSwitch's name and product shape were inspired by [CC Switch](https://github.com/farion1231/cc-switch). The two projects are independent and do not imply endorsement either way.

## License

[MIT](./LICENSE)
