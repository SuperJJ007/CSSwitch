# v0.7 UI redesign: Science multi-model alias Stage 0

Date: 2026-07-18

Status: installed-runtime isolated compatibility evidence. This is not live-provider evidence and does not use a real provider credential.

## Target and isolation

- Source branch/worktree: `codex/v070-ui-redesign` at `bd1f3ab` plus the current protected dirty tree.
- Science executable: installed Claude Science `0.1.18-dev.20260709.t211149.shab3f5130`.
- Test: ignored Rust E2E `isolated_science_accepts_many_csswitch_aliases_and_refreshes_after_restart`.
- Isolation: fresh temporary outer HOME, dedicated Science data-dir, virtual local login, dynamic loopback Science/preview/mock ports, stubbed `security`, and a temporary Playwright session.
- The test rejects port `8765`, does not read `~/.claude-science`, `~/.csswitch`, `~/.codex`, Keychain, or a real provider credential, and stops Science using the exact executable plus test data-dir identity.

## Probe

The loopback Anthropic mock initially published two legacy-shaped entries:

- `claude-csswitch-codex-stage0-old-a`
- `claude-csswitch-codex-stage0-old-b`

After a controlled Science restart, the same mock published six provider-scoped entries using:

```text
claude-csswitch-qwen-stage0-model-<n>-<digest>
```

The Playwright session opened the real Science project UI, inspected `More models`, selected model 5, submitted a harmless prompt, and the mock recorded the request model.

## Result

- Both old Codex display names were visible before restart.
- After restart, neither old display name remained visible.
- All six Qwen-scoped display names were visible; the catalog did not collapse to three role slots.
- Selecting model 5 changed the Science model button to its display name.
- The mock received the exact selected `claude-csswitch-qwen-...` selector id in the Anthropic request body.

Final command:

```bash
env \
  PATH=/Users/superjj/.rustup/toolchains/stable-aarch64-apple-darwin/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin \
  CSSWITCH_REAL_SCIENCE_MODEL_ALIAS_E2E=1 \
  CSSWITCH_PLAYWRIGHT_CLI=/Users/superjj/.npm/_npx/31e32ef8478fbf80/node_modules/.bin/playwright-cli \
  CSSWITCH_PLAYWRIGHT_BROWSERS_PATH=/Users/superjj/Library/Caches/ms-playwright \
  cargo test --manifest-path desktop/src-tauri/Cargo.toml \
    isolated_science_accepts_many_csswitch_aliases_and_refreshes_after_restart \
    -- --ignored --nocapture
```

Result: `1 passed`, exit `0`, 176.55 seconds.

An earlier attempt reached a healthy temporary Science daemon but timed out while the `npx` wrapper tried to initialize Playwright under the cleared test HOME. The durable test now accepts an explicit preinstalled Playwright CLI and read-only browser cache path; no product runtime behavior changed for that tooling correction.

## Decision

Freeze static selector alias v1 as deterministic, persisted `claude-csswitch-<provider namespace>-...`. Role-shaped synthetic ids are not required for arbitrary catalog entries. Official Claude role/date aliases remain separate compatibility inputs resolved through explicit role bindings.
