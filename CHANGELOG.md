# Changelog

All notable changes to `minicoding-rs` are documented here.
Versions follow [Semantic Versioning](https://semver.org/).

## [1.0.0] - 2026-09-04

First stable release. The full feature set is in place and the API surface is considered stable:

- Agent loop with parallel/serial tool scheduling, plan mode, and typed subagents.
- Permission model (policy + prompter) with an un-overridable builtin blacklist, and OS-level sandbox (Landlock/seccomp on Linux, Seatbelt on macOS, Job Object on Windows).
- Context management with four-level compaction, circuit breaking, and long/auto/session memory.
- Multi-provider LLM support (OpenAI/Anthropic/Ollama), MCP client and server, hooks with asyncRewake, file-change journal with `/undo`.
- Four frontends sharing one Rust backend: CLI, TUI, HTTP/SSE server, and Web/desktop (Tauri).
- Declarative `SKILL.md` skills, OpenTelemetry tracing, JSONL storage, and audit logging.

## [0.5.2] - 2026-09-01

### Performance
- Web: optimized streaming render path (batched token flush, memoized message rows).

## [0.5.1] - 2026-09-01

### Fixed
- Web: render reasoning inline at its message position instead of appending to the end.

## [0.5.0] - 2026-09-01

### Added
- Skills: declarative `SKILL.md` skill system with progressive disclosure.
- Web: persist reasoning across `message_appended`; readability and observability timeline for tool activity.

### Fixed
- Web: blank AI-message rendering; tool-call readability.

## [0.4.0] - 2026-08-31

### Added
- CLI: `memory list`/`read`/`clear` subcommands.

### Fixed
- Sandbox: fail-closed when unavailable; `full-access` requires explicit confirmation.
- Web: out-of-the-box 401 on first run; token input in the UI.
- Context: output reservation driven by provider `max_output`; single source of truth for `context_window`.
- Providers: capability drift guard; `Retry-After` clamped to `max_backoff_ms`.
- Core: first-message role normalization (Anthropic requires leading `user`).
- Storage: directory permissions tightened to `0700`.
- Server: CORS hardened with `Vary: Origin`.
- Docs: honest disclosure for Windows/seccomp claims.

## [0.3.11] - 2026-08-31

### Security
- Memory/AGENTS.md injection boundary escaping.
- Consistent redaction coverage (`fs.grep` now redacts).
- Permission persistence scoped by workdir with TTL expiry.

### Reliability
- Feature gating effective; capability set discoverable.
- Compression trigger and threshold use a single measure (no false circuit-break on healthy sessions).

### Fixed
- TUI: panic hook forces `ratatui::restore`.
- Providers: Ollama `tool_call` index collision across lines.
- Web: React `ErrorBoundary` prevents blank screens.

## [0.3.10] - 2026-08-30

### Security / Reliability
- Storage: crash half-line freeze handling; `mkdir` path-escape blocked.
- MCP: approval by command fingerprint (not name only); remote tool-result output cap; stderr injection guard.
- Policy: shell blacklist hardened against adversarial payloads; read-only/harmless commands auto-allowed in Default mode.
- Sandbox: network hint on legacy kernels; `.git` transform protection.

### Fixed
- Providers: `ApproxTokenizer` underestimation closed; `context_window` overridable; Ollama `num_ctx`-aware window.
- Context: calibration measure unified; heuristic summary marks truncated messages; low-quality awareness.
- Memory: AutoMemory staleness governance (source-file fingerprint).
- Server: body limit; `~` path rejection; `create_new` for writes.
- Storage: `SeqGap` downgraded to warn-and-continue; `acquire_blocking` timeout; JSONL single-line cap.

### New Features
- TUI: `/tokens` `/status` `/model` `/plan` `/undo` slash commands.
- Web/TUI: tool-result folding, stuck-turn cancel, persistent input history.

## [0.3.9] - 2026-08-28

### Performance
- Core: tool-call wave scheduling — adjacent read-only calls run in parallel.

### Security / Reliability
- Journal: symlink-escape blocked; shell hang/redaction fixes; C-22 second confirmation wired through Web.
- Context: emergency `ContextLength` compaction; capability probing.

### Fixed
- Server: permission amplification on resolve; turn queue cap.
- Sandbox: seccomp `io_uring`; retrieval overwrite warning; MCP tool audit.

## [0.3.8] - 2026-08-28

### Security
- Credential allowlist completed; MCP tool auditing.
- Anthropic capabilities aligned; retry jitter uses PRNG.
- `web.search` per-hop redirect handling; `web.fetch` configurable body cap.

### Fixed
- Context revision staleness guarded; docs disclose behavior accurately.

## [0.3.7] - 2026-08-28

### Security
- Memory/policy/sandbox/providers/tools: security P0/P1 fixes.
- Storage/server: persistence and frontend fixes.
- Context/memory: compaction and retrieval fixes.

### Fixed
- Cross-platform CI: `subtract_denied` dead code; stale generated DTOs.
- Memory: `@import` containment uses `canonicalize_within` (macOS/Windows).

## [0.3.6] - 2026-08-27

### Security
- Policy/sandbox/hooks/memory: security P0/P1 fixes.
- Tools/providers/core: tool and provider fixes.
- Journal: byte budget (32 MiB) in addition to entry count.

### Fixed
- Cross-platform CI: Windows dead code; macOS test sync.
- Memory: `@import` containment Windows false-positive.

## [0.3.5] - 2026-08-26

### Security
- Policy: dangerous-command blacklist adversarial hardening.
- Runtime: correctness fixes across providers, context, server, MCP, hooks.

### Fixed
- Frontends: permission dialog options wired through; sidecar crash notification.

## [0.3.4] - 2026-08-25

### Security
- Policy/sandbox/journal/storage/core: security fixes.
- Runtime: core/tools/mcp/server/cli/sdk fixes.
- Context/memory: budget, pairing repair, half-open breaker, memory hardening.

### Fixed
- Providers/tools/mcp: provider-layer fixes.
- Frontends: four-form (server/web/cli/tui/desktop) fixes.

## [0.3.3] - 2026-08-25

### Fixed
- Windows compile failures; typos gate.
- Sandbox: `JobHandle` `Send`/`Sync`.

## [0.3.2] - 2026-08-25

### Fixed
- Desktop: sidecar WebView origin allowlisted (all HTTP requests were failing).

## [0.3.1] - 2026-08-24

### Fixed
- Desktop: window close exits app and terminates sidecar (PID fallback).
- Web/server: new-session failures surfaced; workdir pre-validated.

## [0.3.0] - 2026-08-24

### Added
- Hooks: full lifecycle wiring; MCP supervisor with read-only default.
- Policy/core/web: `AllowAlways`/`DenyAlways` persistence across four frontends.
- CLI/core: `/model` command, `@file` references, path-granular decision persistence.
- Providers/core: cost engineering — prompt caching, thinking config, `count_tokens` calibration.
- Sandbox: Linux/macOS child processes default to no network under ReadOnly/WorkspaceWrite.

### Fixed
- TUI/web/desktop: scrollback + visible-region render, event guards, Desktop CSP.

## [0.2.33] - 2026-08-22

### Added
- Server/web: protocol contract and observability.
- Policy/tools: permission and resource hardening.

### Fixed
- Core/policy/hooks: architecture governance.

## [0.2.31] - 2026-08-21

### Added
- Storage: concurrency-safe message stream with single-bad-line tolerance.
- Core: session step-boundary event persistence.
- Context: compression reference chain traceability.
- Sandbox: structured denial passthrough.
- Providers: credential re-resolution + stale-config write protection.
- Tools: render-intent output declarations.

### Fixed
- Providers: SSE event-boundary CRLF normalization.
- Core: dangling `tool_calls` backfill.

## [0.2.30] - 2026-08-19

### Added
- Web: settings panel for model parameters and context configuration.
- Web: light/dark theme refresh.

## [0.2.29] - 2026-08-17

### Fixed
- Server: NDJSON/ACP deadlock during a turn (permission commands not consumed).
- CLI/policy: non-interactive auto-approve; relative-path fallback.

### Added
- Web: input-history navigation with arrow keys.

## [0.2.x] - 2026-08-05 → 2026-08-15

### Added
- Desktop (Tauri): cross-platform packaging, sidecar lifecycle, in-app settings.
- Web: optimistic message update; task snapshot over HTTP; runtime tool progress/cancel; detailed logging.
- Sandbox: init-failure fallback prompt to run outside sandbox.
- Server/web: session lazy restore; Plan-mode entry.

### Fixed
- Numerous desktop build/runtime issues across macOS/Windows/Linux.
- SSE UTF-8 mojibake; sidecar port output.

## [0.2.0] - 2026-08-04

### Added
- Desktop release pipeline; `cargo dist` cross-platform binaries.
- Observability improvements.
- Replay; macOS/Windows sandbox; backup and worktree; config watcher; contributors; ACP/LSP adapters; protocol and SDK.

## [0.1.0] - 2026-08-03

### Added
- Initial feature set: single-turn CLI, agent loop, tools, memory, tasks, MCP, sandbox, hooks, plan, subagents, providers, TUI.
