<p align="center">
  <img src="docs/assets/social-preview.png" alt="CSSwitch" width="760">
</p>

<p align="center">
  <img src="https://img.shields.io/badge/license-MIT-blue.svg" alt="MIT License">
  <img src="https://img.shields.io/badge/platform-macOS%20Apple%20Silicon-1d1d1f.svg" alt="macOS Apple Silicon">
  <img src="https://img.shields.io/badge/built%20with-Tauri%202-C25A34.svg" alt="Tauri 2">
</p>

<p align="center">
  <a href="./README.md">简体中文</a> ·
  <a href="./README.en.md">English</a>
</p>

# CSSwitch

CSSwitch is a local model switcher for Claude Science. It routes Science inference requests to your own third-party model API, so you can use DeepSeek, Qwen, Kimi, MiniMax, GLM, OpenRouter, relay providers, or custom compatible endpoints inside Science without a Claude subscription.

It is built for more than developers. You need Claude Science, a third-party API key, and the CSSwitch desktop panel: create a profile, make it active, then click "一键开始" (Start).

> The current app mainly targets macOS Apple Silicon. Because the app is not notarized yet, macOS may ask you to right-click and choose "Open" the first time.

[Download latest release](../../releases/latest) · [Changelog](./CHANGELOG.md) · [Report a bug](https://github.com/SuperJJ007/CSSwitch/issues/new?template=bug_report.yml) · [Request a feature](https://github.com/SuperJJ007/CSSwitch/issues/new?template=feature_request.yml)

## Contents

- [Why CSSwitch exists](#why-csswitch-exists)
- [What it can do](#what-it-can-do)
- [Quick start](#quick-start)
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
- It prepares a locally generated launch ticket for Science without copying your real Claude login.
- It forwards Science model requests to the provider you choose.
- It translates between Anthropic Messages API and OpenAI-compatible APIs when needed.
- It keeps an "Official Claude" mode so subscribers can switch back to the real Science app.

In short: CSSwitch is to Claude Science what CC Switch is to Claude Code, but Science also needs a launch-ticket and sandbox layer.

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

**For advanced users**

- Supports native Anthropic-compatible endpoints, OpenAI Chat Completions-compatible endpoints, and OpenAI Responses-compatible endpoints.
- Supports custom `base_url`, model names, and relay providers.
- Native Anthropic endpoints such as DeepSeek, Kimi, and MiniMax are passed through when possible to preserve tool use, thinking, and streaming behavior.
- Qwen and custom OpenAI endpoints are translated by the local proxy.
- Local config and logs make debugging and issue reports easier.

## Quick start

Before starting, make sure you have:

- [Claude Science](https://claude.com)
- A macOS Apple Silicon device
- A working third-party model API key
- `python3` (the current proxy still needs it; moving this into Rust is planned)

1. Download the latest `CSSwitch_*.dmg` from [GitHub Releases](../../releases/latest).
2. Drag CSSwitch into Applications.
3. If macOS blocks the first launch, right-click the app and choose "Open".
4. Keep the top mode set to "第三方模型" (third-party model).
5. Click "+ 新建" (New), choose a provider, and enter your API key, model, and `base_url` when required.
6. Click "创建" (Create), then choose "设为当前" (Set active) on the profile.
7. After verification succeeds, click "一键开始" (Start).
8. CSSwitch starts the isolated Science instance and opens it in your browser.

If you have a Claude subscription and want the normal official Science experience, switch to "官方 Claude" (Official Claude). CSSwitch will tear down the third-party proxy path and open the real Science app.

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

CSSwitch includes a read-only capability catalog that makes known compatibility boundaries explicit across providers, tool use, MCP/skills, Science versions, and transport behavior. Runtime `status` diagnostics return the catalog rules matched by the current profile plus fixed boundary rules, which helps explain why a configuration is handled a certain way and which capabilities are diagnostic-only or degraded.

This catalog is for diagnostics and observability. It is not proof that a live provider, real Claude account state, Science GUI E2E flow, DMG signing/notarization, or official hosted capability has been verified. A catalog rule id means CSSwitch records that rule or boundary; it does not mean external providers, Anthropic-hosted MCP, Directory connectors, or remote skills are fully verified or fixed.

Status lights are local observations only. For example, the sandbox light is local HTTP health, not proof that the port has been identity-verified as the CSSwitch sandbox Science instance. `Doctor` skips the real `~/.claude-science` path by default; checking whether the real HOME path exists requires explicitly setting `CSSWITCH_DOCTOR_CHECK_REAL_HOME=1`.

## How it protects your real account

CSSwitch's core boundary is simple: third-party model mode keeps credentials, data directories, and proxy routing inside the sandbox. It does not take over your real Claude account.

- It does not copy, read, or modify real Claude login credentials, OAuth tokens, account state, or user data.
- On first sandbox initialization, it may read-only clone Science runtime assets from the real `~/.claude-science` path, such as `bin`, `conda`, `runtime`, and `seed-assets`; these are not account credentials or conversation data.
- The isolated Science instance uses its own HOME, ports, and data directory.
- Third-party API keys are stored in `~/.csswitch/config.json` with `0600` file permissions.
- Keys are passed to the local proxy through environment variables, not command-line arguments or logs.
- The proxy only listens on `127.0.0.1` and validates requests with a path secret.
- Incoming Science `Authorization` / `x-api-key` headers are stripped before CSSwitch injects your configured third-party key.
- Official Claude mode tears down the third-party proxy path before handing you back to the real Science app.

## Current limitations

CSSwitch is not an official Claude service, and its locally generated launch ticket does not grant Anthropic account privileges. These are current architectural limits:

- Anthropic-hosted remote MCP services are unavailable, including `pubmed`, `clinical-trials`, `chembl`, `biorxiv`, and other `*.mcp.claude.com` services.
- Directory connectors, remote plugins, and cloud features that require real Claude account authorization may show session expired, unavailable, or skipped.
- Third-party models differ in tool use, long context, thinking, image, and streaming compatibility. Native Anthropic endpoints are usually more reliable than OpenAI translation paths.
- The macOS package is not notarized yet, so the first launch requires manual approval.
- The current runtime still needs `python3` for the proxy. Moving to a Rust single-binary proxy is on the roadmap.

Known issues and roadmap notes live in [docs/known-issues.md](./docs/known-issues.md).

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

```bash
cd desktop
npm install
npm run tauri dev
```

Common checks:

```bash
bash test/run_all.sh
bash test/run_all.sh --require-release-ready

python3 -m unittest test.test_proxy_units test.test_provider_policy test.test_proxy_packaging -v
(cd desktop/src-tauri && cargo test)
node --check desktop/src/main.js
```

More development notes:

- [desktop/README.md](./desktop/README.md)
- [docs/DEVELOPMENT.md](./docs/DEVELOPMENT.md)
- [docs/provider-support.md](./docs/provider-support.md)
- [docs/verified-facts.md](./docs/verified-facts.md)

## Risk and disclaimer

- This project is for personal learning and research. Use it at your own risk.
- CSSwitch is not affiliated with, endorsed by, or partnered with Anthropic.
- Inference requests are sent to the third-party model service you configure and pay for.
- The locally generated Science launch ticket does not contain real Anthropic credentials and does not grant official Anthropic account permissions.
- Science may still try to access built-in profile, account, or service-discovery endpoints during startup. In third-party model mode, CSSwitch isolates or fast-fails those requests where possible, so this README intentionally avoids absolute claims such as "never contacts Anthropic".
- Analysis of Science's login-token encryption format and the local launch-ticket implementation may involve terms-of-service or legal questions. Applicability should be assessed by a qualified professional.
- The software is provided "as is", without warranty of any kind.

## Acknowledgements

CSSwitch's name and product shape were inspired by [CC Switch](https://github.com/farion1231/cc-switch). The two projects are independent and do not imply endorsement either way.

## License

[MIT](./LICENSE)
