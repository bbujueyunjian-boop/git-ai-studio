# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
Version bumps are manual — see [CONTRIBUTING.md](CONTRIBUTING.md#releasing-a-new-version-maintainer-only) and [ADR-008](docs/adr/0008-conventional-commits-release-tool.md).

## [Unreleased]

## [0.5.1] - 2026-09-08

### Fixed

- Uninstall now runs the official hook removal command and keeps the installation when hook cleanup fails.
- Disabling or updating Claude hooks preserves user hooks in the same matcher group and retains the original configuration backup.
- Captured commands now terminate their child process on timeout or cancellation; the timeout also covers blocked input and output.
- Checkpoints refresh when switching repositories without leaving the page.
- Daemon diagnostics accept unlocked residual lock files as idle and no longer kill processes or delete runtime files when the lock holder is unknown.
- Closing the main window in exit mode now exits the application even when the hidden desktop companion window exists; tray mode continues to support restoring the main window.
- Invalid or unreadable configuration files now report errors instead of being overwritten with defaults; git-ai configuration updates require a successful backup.
- Codex diagnostics respect the explicitly selected `hooks_json` format as well as the default inline TOML format.

## [0.5.0] - 2026-08-17

### Added

- The global repository switcher now lets users clear recent repository history after confirmation without removing source code, scan roots, the Dashboard aggregate set, or the currently selected repository.

### Changed

- Repository management now keeps the active scan scope visible and moves scan-root editing into a dedicated dialog. The duplicate recent-repository list was removed from the Repository page so recent history has a single home in the global switcher.

## [0.4.0] - 2026-08-15

### Added

- Commit Attribution now shows each changed file's AI share as AI-attributed added lines divided by all lines added to that file. The file row also exposes AI, human, and unknown additions, while deletions remain outside the denominator.
- The Commit Attribution summary now states the current repository, branch, HEAD-reachable commit window, active filters, and exact formulas for all five headline metrics.

### Fixed

- Clearing the repository aggregate set now immediately removes the Dashboard's historical and working-tree roll-ups, so repositories and numbers that were just removed no longer remain visible.

## [0.3.6] - 2026-08-15

### Fixed

- Per-file AI line badges in Commit Attribution now include the `s_<session>::t_<trace>` note format used by current git-ai releases, instead of appearing empty for Codex and other session-based agents.

## [0.3.5] - 2026-06-07

### Fixed

- Launching the app a second time no longer spawns a duplicate process. Tauri allows multiple instances by default, so the app could previously be opened several times — producing two tray icons, two desktop-pet windows when the pet was enabled, and concurrent writes to the same `config.json`. A single-instance guard now hands off to the running instance and focuses its main window instead of starting a new one.
- Claude Code / Codex / `npm` installed under nvm, Volta, Bun, a custom npm prefix, Homebrew, or the official native installer (`~/.local/bin`) are now detected even when the login-shell PATH probe is skipped (PATH already resolves `npm`) or fails (non-POSIX default shell, slow or failing shell rc). The detector falls back to scanning these well-known install directories, and prepends the resolved binary's own directory to the subprocess PATH so the `#!/usr/bin/env node` shebang can find `node`.

## [0.3.4] - 2026-06-05

### Fixed

- AI coding tools (Claude Code / Codex) and `npm` are now detected even when the app is launched from Finder/Dock (macOS) or Explorer (Windows). GUI-launched apps inherit a minimal PATH that omits nvm/Homebrew/fnm install locations, so tools installed there previously showed as "not installed". The app now resolves the real PATH from the login shell (macOS/Linux) or the live registry PATH (Windows), falls back to a POSIX shell when the default shell is non-POSIX (fish/csh), and "Re-check" re-reads it at runtime without restarting the app.

## [0.3.3] - 2026-06-05

### Added

- The Diagnostic page can now install and uninstall the Claude Code (`@anthropic-ai/claude-code`) and Codex (`@openai/codex`) CLIs via global npm, with an optional version to pin (defaults to the latest).

## [0.3.2] - 2026-06-03

### Added

- The repository setup guide can now batch-add multiple repositories to an aggregation set in one step, instead of one at a time.

### Fixed

- "Fix this" for an individual agent now re-probes after running `git-ai install` and reports an honest failure (with the real reason) instead of always reporting success.
- Codex diagnostics now detect legacy `~/.codex/hooks.json` hooks and explain that inline `config.toml` hooks require git-ai 1.4.8+ (the release that migrated Codex to inline TOML), guiding the user to upgrade git-ai instead of silently staying red.
- The Install page no longer disables "Install / Upgrade to latest" when the GitHub Releases API is unreachable (rate-limited or blocked); the official install script resolves the latest version on its own.
- Commit rows now reveal their full details (complete message + SHA) on hover, with the hover text fully localized (Simplified Chinese / English).
- The branch dropdown now shows complete per-row hover info (full branch name + SHA) and is no longer truncated by an inner `title` attribute.
- Closing the app no longer hangs while the cc-switch guardian watcher shuts down (the debouncer is now stopped and dropped before joining the worker, and the lock is released before the handle is dropped).
- The desktop companion ("Ink pet") no longer occasionally flashes a black frame on open (the pet window now gets a transparent background color and an initial size that matches its runtime size).

## [0.3.1] - 2026-06-01

### Fixed

- Desktop companion ("Ink pet"): fixed the transparent window rendering a black background and a stray scrollbar (mark the window before mount + flatten the layout with declarative CSS).

## [0.3.0] - 2026-06-01

### Added

- Hook detection for two more AI agents — **Gemini** and **Pi** (backend probes plus frontend status), bringing the agent matrix to six.

### Changed

- Line-level Blame is now part of commit attribution: per-line authorship opens as a drill-down dialog instead of a separate route (see [ADR-013](docs/adr/0013-blame-into-commit-attribution.md)).
- Completed bilingual (Simplified Chinese / English) i18n across the remaining pages — Diagnostics, Dashboard, Commits, Settings, People, Notes, and the repo setup guide.
- Streamlined the Diagnostics page to a health-first layout (dropped the summary card and the raw-report drawer) and trimmed the Settings / People / Notes headers into click-to-reveal info.
- The sidebar footer version is now read at runtime instead of being hardcoded, so it can no longer drift.

### Fixed

- The desktop pet's right-click menu and hover bubble now follow the main window's language (language is carried in the one-way pet-state stream).
- `git-ai debug` is no longer invoked with the `report` sub-argument that upstream removed (it was being rejected).
- An empty checkpoint set under the current HEAD is treated as a normal empty state (neutral info) instead of being flagged as an actionable problem.

### Removed

- The login-state diagnostic check — the local tool does not require login.

## [0.2.0] - 2026-05-29

### Added

- Desktop companion ("Ink pet") — an opt-in (default off), always-on-top floating widget that renders your live AI-authorship rate as a two-tone ink ball: purple ink (AI) on the outer ring, blue ink (you) at the core, so the shape *is* the data (color + position dual-encoded, colorblind-readable). Ships 3 built-in themes and a hover bubble; only the daemon-unhealthy state may raise an OS notification. See [ADR-011](docs/adr/0011-desktop-companion-ink-pet.md).

## [0.1.0] - 2026-05-29

### Added

- Initial open-source release: a local desktop dashboard for AI code authorship, built on top of the [`git-ai`](https://github.com/git-ai-project/git-ai) CLI.
- Views: Dashboard, Commits (per-commit stats), People (per-author), Blame (line-level), git notes, Checkpoints.
- Official `git ai install-hooks` integration for Claude Code / Cursor / Codex / OpenCode.
- Bilingual UI (Simplified Chinese / English) via i18next, with an in-app language switcher.
- macOS (universal `.dmg`), Linux (`.AppImage` + `.deb`, x86_64 + ARM64), and Windows (`.msi`) builds.
- OS-native notifications (via `tauri-plugin-notification`) for low AI-share and daemon-health alerts — opt-in, no webhook, no cloud.
- `refs/notes/ai` fetch/push sync through the upstream CLI.

### Changed

- UI restyled to a Linear-inspired minimal aesthetic on Tailwind v4 + shadcn/ui.

### Removed

- Self-hosted hook server (Windows scheduled task + VBS shim + Node HTTP server). Hooks now go exclusively through the official `git ai install-hooks`.
- Feishu webhook push (replaced by OS-native notifications).

[Unreleased]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.5.1...HEAD
[0.5.1]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.3.6...v0.4.0
[0.3.6]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.3.5...v0.3.6
[0.3.2]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.3.1...v0.3.2
[0.3.1]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/bujueyunjian/git-ai-studio/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/bujueyunjian/git-ai-studio/releases/tag/v0.1.0
