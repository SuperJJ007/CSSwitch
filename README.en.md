<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.5.0"><img src="https://img.shields.io/badge/release-v0.5.0-2ea44f.svg" alt="CSSwitch v0.5.0"></a>
  <img src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-1d1d1f.svg" alt="macOS Apple Silicon">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
</p>

<p align="center">
  <a href="./README.md">简体中文</a> ·
  <a href="./README.en.md">English</a>
</p>

# CSSwitch

CSSwitch is a local configuration converter for Claude Science. It translates Science inference requests and connects them to your own model API, including DeepSeek, Qwen, Kimi, MiniMax, GLM, OpenRouter, relay providers, or custom compatible endpoints.

It is built for more than developers. You need Claude Science, a third-party API key, and the CSSwitch desktop panel: create a profile, make it active, then click "一键开始" (Start).

> The current app mainly targets macOS Apple Silicon. Because the app is not notarized yet, macOS may ask you to right-click and choose "Open" the first time.

[Download latest release](../../releases/latest) · [Documentation](./docs/README.md) · [Changelog](./CHANGELOG.md) · [Report a bug](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [Request a feature](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

> **0.5.0:** In third-party mode, one combined local connector installs or uninstalls external Skills. Installation requires an exact public GitHub directory URL and, after Science approval, performs the bounded copy, native Agent attachment, and verification. Uninstall quarantines only CSSwitch-owned imports before native detachment. This release also avoids redundant Science version and Skill-route work on repeated starts and adds an explicit opt-in for isolated Science to use the system `~/.ssh/config`. See the [External Skill bridge](./docs/features/external-skill-bridge.md), [system SSH contract](./docs/features/system-ssh.md), and [architecture contract](./docs/architecture/overview.md).

## Contents

- [Why CSSwitch exists](#why-csswitch-exists)
- [What it can do](#what-it-can-do)
- [Quick start](#quick-start)
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
  -> DeepSeek / Qwen / Kimi / MiniMax / GLM / OpenRouter / custom endpoint
```

## What it can do

**For everyday users**

- Manage multiple model profiles from a desktop panel instead of editing environment variables.
- Save multiple profiles for the same provider, such as different keys, models, or relay URLs.
- Verify a key before making a profile active; failed checks do not silently switch your active setup.
- Click "一键开始" (Start) to launch the proxy, prepare the sandbox, and open Science.
- Show the actual selected model name in Science instead of a vague `claude` or `opus` label.
- Switch back to "Official Claude" without interfering with your real Claude login.
- Reuse Science's persistent data-dir; Skill state is not a CSSwitch startup gate. Version 0.5.0 can install a Skill from an exact public GitHub URL and safely quarantine CSSwitch-owned imports through the same connector.
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
- A working third-party model API key
- No separate Python runtime is required; CSSwitch bundles its Rust inference gateway

1. Download the latest `CSSwitch_*.dmg` from [GitHub Releases](../../releases/latest).
2. Drag CSSwitch into Applications.
3. If macOS blocks the first launch, right-click the app and choose "Open".
4. Keep the top mode set to "第三方模型" (third-party model).
5. Click "+ 新建" (New), choose a provider, and enter your API key, model, and `base_url` when required.
6. Click "创建" (Create), then choose "设为当前" (Set active) on the profile.
7. After verification succeeds, click "一键开始" (Start).
8. CSSwitch starts the isolated Science instance and opens it in your browser.

CSSwitch does not choose a Science version for you. Normal startup uses the currently installed Claude Science App. If the App is missing, the panel shows an exact readable cache version and asks whether to use it for this launch only, or to open the official download page. That choice is not persisted, and a later detected App automatically becomes the default again.

To use Science with its official service configuration, switch to "官方 Claude" (Official Claude). CSSwitch will tear down the third-party proxy path and open the real Science app.

## Upgrading from an older version

Version 0.5.0 keeps the existing v2 configuration format and reuses `~/.csswitch/sandbox/home/.claude-science`, so existing Science organizations, projects, and Skills are not migrated or overwritten. Legacy CSSwitch Skill store/inventory files remain untouched but no longer participate in startup; external `~/.claude/skills` trees are not synchronized into Science.

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
- Keys are not displayed in application logs, and the local gateway listens only on loopback.
- “Allow isolated Science to use system SSH configuration” is off by default. When enabled, it only makes real `~/.ssh/config` available to system OpenSSH calls made by Science. CSSwitch does not copy or link all of `.ssh`, start `sshd`, change the firewall, or expose a `0.0.0.0` listener. Native OpenSSH `Include`, key, Agent, and command rules still apply, so this is an explicit trust grant.
- New isolated Science launches prefer the binary from the locally installed official Claude Science app. If the App is unavailable, a readable retained sandbox copy is used only after explicit one-launch authorization; the choice is not persisted. CSSwitch does not download Science and keeps `--no-auto-update`.
- Official Claude mode tears down the third-party proxy path before handing you back to the real Science app.

## Current limitations

CSSwitch is not an official Claude service, and third-party model mode does not grant Anthropic account privileges. These are current architectural limits:

- Anthropic-hosted remote MCP services are unavailable, including `pubmed`, `clinical-trials`, `chembl`, `biorxiv`, and other `*.mcp.claude.com` services.
- Directory connectors, remote plugins, and cloud features that require real Claude account authorization may show session expired, unavailable, or skipped.
- Science's native GitHub import, Skill publishing, and draft deletion may still query the Anthropic account catalog. CSSwitch does not emulate OAuth or that catalog; version 0.5.0 supports only exact public GitHub URLs and quarantine removal, not deterministic name search, updates/overwrite, permanent-delete/restore UI, private repositories, or publishing.
- Third-party models differ in tool use, long context, thinking, image, and streaming compatibility. Native Anthropic endpoints are usually more reliable than OpenAI translation paths.
- The macOS package is not notarized yet, so the first launch requires manual approval.
- The inference gateway is a bundled Rust sidecar; no runtime Python fallback is shipped.

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
