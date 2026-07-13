# Release gates

Passing a lower layer never implies a higher layer passed.

1. **Source:** focused unit/static tests, Rust tests where the environment permits, clippy, and `git diff --check`.
2. **Artifact:** build the intended app/sidecar from the exact release commit and verify embedded versions and registered commands.
3. **Installed copy:** install only the candidate artifact into an isolated test location; do not overwrite `/Applications/CSSwitch.app` during development.
4. **Installed runtime:** use temporary HOME/data-dir and dynamic ports to verify Gateway ownership, one-click launch, reuse, stop, and reopen.
5. **Live provider:** when explicitly authorized, verify a real provider separately from loopback mocks and report model/tool compatibility precisely.
6. **Distribution:** verify code signature, notarization, Gatekeeper behavior, hashes, and the final uploaded artifact.
7. **Published release:** only after the public tag, release entry, assets, README, CHANGELOG, upgrade notes, and known limitations agree.

For upgrades from 0.4.3 or later, the candidate must reuse `~/.csswitch/sandbox/home/.claude-science` and leave existing Science organization/Skill data and legacy CSSwitch Skill store files untouched. External Skill trees, legacy inventory corruption, and provider Skill catalog availability must not affect startup.

For every candidate tested against a new Science App, record the actual executable path and `--version`, reused data-dir, live version-runtime directory, and port identity. Verify that App + stale cache selects App; App missing never selects cache without one-shot user authorization; unknown cache version cannot launch; replacing the App advances the next launch without modifying cache or persistent state. Then run the natural-language route and install/attach/load/restart/uninstall/detach E2E, while confirming all Skill writes stay under the active organization and route/MCP failure remains warning-only.

No release, app replacement, tag, push, or remote-branch deletion is implicit in successful development tests.
