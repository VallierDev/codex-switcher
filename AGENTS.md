# Project agents — codex-switcher

Tauri 2 desktop app that proxies codex CLI / Claude Code traffic across
multiple ChatGPT / Relay accounts. Routing rules from
`~/.claude/CLAUDE.md` and `~/.codex/AGENTS.md` apply here too.

## Local conventions

- Rust + React. `npx tauri dev` for live reload, `npx tauri build
  --bundles app` for production .app.
- Source: `src-tauri/src/` (Rust) + `src/` (React/TS). Frontend: Vite,
  no UI lib (pure CSS).
- Don't introduce dependencies you can do without.

## Architecture

- `src-tauri/src/proxy.rs` — main HTTP+WS proxy. Get familiar with
  `handle_request` / `handle_websocket` / `get_upstream` (3-branch:
  ChatGPT / OpenAI key / Relay) before touching it.
- `src-tauri/src/account.rs` — `AccountKind` (Legacy / ChatgptOauth /
  OpenaiKey / Relay) + `AppSettings`.
- `src-tauri/src/output_compress.rs` — Phase-1 shell-output compressor
  hooked into both Codex WS and Claude SSE paths in proxy.rs.
- `src-tauri/src/usage.rs` — quota fetchers per account kind.

## When editing

- After ANY proxy.rs change, restart the running Codex Switcher.app to
  reload (the proxy is in-process).
- Settings UI lives in `src/components/Settings.tsx`. Mirror the existing
  toggle pattern — don't invent new layouts.
- Two macs run this: local + mini mac (192.168.2.6 LAN / mini-mac-zt
  ZeroTier). Big changes go through `tar -czf + scp` after replacing
  /Applications/Codex Switcher.app locally.

## GitHub synchronization

- This repository's canonical remote is `origin`; check the remote URL and
  fetch `origin/main` before starting a substantial change.
- After every code or documentation modification, fetch `origin/main` again
  and compare it with the current branch (`git status`, `git log`, and the
  relevant diff). Report if GitHub has commits that are not present locally.
- Never discard uncommitted local work or force-reset the branch to GitHub.
  If both sides changed, preserve the local work and resolve/rebase only with
  an explicit, reviewable operation.
- Before declaring the task complete, verify the working tree and branch
  relationship with GitHub. If the user asked to synchronize/publish, commit
  the intended changes and push to `origin/main`; otherwise, leave publishing
  to the user and clearly report the local/remote state.

## When NOT to use glance

This project deals with proxying chat completion APIs — don't ask glance
to "research" the OpenAI / Claude wire format. Read the official docs
yourself or look at `proxy.rs` directly.
