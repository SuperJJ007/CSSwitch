<p align="center">
  <img src="desktop/src-tauri/icons/icon.png" alt="CSSwitch" width="112">
</p>

<p align="center">
  <a href="./LICENSE">MIT License</a> · <strong>CSSwitch v0.8.2</strong> · macOS Apple Silicon · Tauri 2
</p>

<p align="center">
  CSSwitch connects Claude Science to your own model APIs.<br>
  Switch between mainstream providers, Codex, and custom compatible endpoints.
</p>

> Linux x64 users can use the [CSSwitch v0.8.1 Linux x64 prerelease](https://github.com/SuperJJ007/CSSwitch/releases/tag/v0.8.1-linux-x64), provided as an amd64 `.deb` package.

<p align="center">
  <img src="docs/assets/csswitch-v0.8-ui-demo.gif" alt="CSSwitch v0.8 series UI demo" width="942">
</p>

---

<p align="center">
  <a href="https://github.com/SuperJJ007/CSSwitch/releases/download/v0.8.2/CSSwitch_0.8.2_aarch64.dmg">Download v0.8.2</a> ·
  <a href="#install-and-start">Install and start</a> ·
  <a href="#providers-and-models">Providers and models</a> ·
  <a href="#skills-and-mcp">Skills and MCP</a> ·
  <a href="./README.md">简体中文</a>
</p>

## Install and start

You need an Apple Silicon Mac, [Claude Science](https://claude.com/download), and either a third-party model API key or a Codex account.

1. Download [`CSSwitch_0.8.2_aarch64.dmg`](https://github.com/SuperJJ007/CSSwitch/releases/download/v0.8.2/CSSwitch_0.8.2_aarch64.dmg) and drag CSSwitch into Applications.
2. Create a profile and enter the API key, model names, and `base_url` when required.
3. Choose **Set active**, then **Start**.
4. Select the model from the model picker at the top of Science.

> The current package is ad-hoc signed and not notarized. If macOS blocks the first launch, right-click CSSwitch in Finder and choose **Open**.

## Providers and models

- **Built-in providers:** DeepSeek, Qwen, GLM, Xiaomi MiMo, SiliconFlow, Kimi, MiniMax, and OpenRouter.
- **Custom endpoints:** Anthropic Messages, OpenAI Chat Completions, and OpenAI Responses-compatible APIs. Exact model names can be entered without discovery.
- **Model selection:** A regular profile can use one model for every role, or separate Quality, Balanced, Fast, and Fable models. Science shows the real model name instead of a `default` placeholder.
- **Codex:** Uses a separate CSSwitch browser login and dynamic account catalog. Native `~/.codex` login is never read or modified.

Provider support for tools, thinking, images, long context, and streaming varies. CSSwitch resolves the active catalog strictly; an unknown model is never silently replaced by another model.

## Skills and MCP

The **Skill & MCP** page lists real Skills from the current Science organization, including source and attachment state. It can import local `.zip` and `.skill` packages, while the CSSwitch connector can install a Skill from an exact public GitHub URL.

CSSwitch only manages content it imported. Name conflicts never overwrite existing content, and bundle removal requires whole-bundle confirmation. See the [external Skill bridge](./docs/features/external-skill-bridge.md) for the complete contract.

## Security and isolation

- Third-party mode uses a separate HOME, data directory, and loopback-only Gateway. It does not read or modify real Claude login or Science data.
- API keys stay in local `~/.csswitch/config.json` with `0600` permissions and are not written to logs.
- Official Claude mode stops the third-party proxy path before opening real Science.
- CSSwitch does not download or update Claude Science. New launches prefer a locally validated runtime snapshot already downloaded by the official updater, then fall back to the currently installed official app.

## Current boundaries

- Public desktop builds currently target macOS Apple Silicon only.
- Third-party mode does not grant Anthropic account privileges; hosted MCP services, directory connectors, and some cloud features may be unavailable.
- Codex remains an off-by-default experiment with one browser-authenticated account.
- The Rust Gateway is bundled; no separate Python runtime is required.

For upgrades, rollback, and detailed limitations, see the [project documentation](./docs/README.md). Please report issues through [GitHub Issues](https://github.com/SuperJJ007/CSSwitch/issues).

## Development

```bash
cd desktop
npm install
npm run tauri dev
```

Run the complete local gate with:

```bash
bash test/run_all.sh
```

[Changelog](./CHANGELOG.md) · [Development and testing](./docs/operations/development.md) · [Release evidence](./docs/evidence/releases/README.md)
