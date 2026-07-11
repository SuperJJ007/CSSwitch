# Changelog

CSSwitch follows semantic versioning. Older release notes remain available on the [GitHub Releases page](https://github.com/SuperJJ007/CSSwitch/releases).

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
