# ClaudeTop — TUI Coding Agent Credits Monitor

## Goal

Build a Rust TUI application using Ratatui that displays coding agent credits/usage for Claude, Codex, and Gemini providers in a dashboard view.

## Providers (v1)

- **Claude** — OAuth token from `~/.claude/` → env var `ANTHROPIC_API_KEY` → config file
- **Codex** — Config from `~/.codex/` → env var `OPENAI_API_KEY` → config file
- **Gemini** — gcloud config → env var `GEMINI_API_KEY` → config file

## Dashboard Layout

```
┌──────────────────── ClaudeTop ─────────────────────┐
│                                                     │
│  Claude (Pro)              Resets in 2h 14m         │
│  ██████████░░░░░░░░░░  48% session (5h window)     │
│  ████░░░░░░░░░░░░░░░░  22% weekly  (7d window)    │
│  Credits: ∞              Cost (30d): $142.30        │
│                                                     │
│  Codex (Pro)               Resets in 4h 02m         │
│  ████████████████░░░░  80% session                  │
│  ██████░░░░░░░░░░░░░░  31% weekly                  │
│  Credits: 240/500        Cost (30d): $87.50         │
│                                                     │
│  Gemini (Pro)              Resets in 1h 45m         │
│  ██████░░░░░░░░░░░░░░  30% session                 │
│  ██░░░░░░░░░░░░░░░░░░  12% monthly                 │
│  Credits: 1,200 RPD      Cost (30d): $23.10         │
│                                                     │
│─────────────────────────────────────────────────────│
│  Last refresh: 2m ago   Next: 3m   [r]efresh       │
│  [q]uit  [p]ause  [c]onfig                         │
└─────────────────────────────────────────────────────┘
```

## Architecture

Three layers:

1. **TUI Layer** (Ratatui + crossterm) — Rendering, input handling, layout
2. **App State** — Provider data, refresh timer, config
3. **Provider Layer** — Auth discovery, API fetching, log parsing

## Key Crates

- `ratatui` + `crossterm` — TUI rendering
- `reqwest` — HTTP (async)
- `tokio` — async runtime
- `serde` / `serde_json` — JSON parsing
- `toml` — config parsing
- `dirs` — cross-platform config/home directories

## Config

`~/.config/claudetop/config.toml`:

```toml
[general]
refresh_interval_secs = 300  # default 5 minutes

[claude]
# token = "sk-..."  # optional override

[codex]
# api_key = "sk-..."  # optional override

[gemini]
# api_key = "AI..."  # optional override
```

## Auth Discovery (per provider)

| Provider | Strategy Order |
|----------|---------------|
| Claude | `~/.claude/` OAuth token → `claude` CLI → env var `ANTHROPIC_API_KEY` → config file |
| Codex | `~/.codex/` config → env var `OPENAI_API_KEY` → config file |
| Gemini | `~/.config/gcloud/` creds → env var `GEMINI_API_KEY` → config file |

## Data Model

Per provider:
- Rate windows (session, weekly/monthly) with used_percent and reset_at
- Credits remaining (optional)
- Plan name (optional)
- 30-day cost total (from local session log scanning)
- Last updated timestamp
- Status: ok | unavailable | not_configured

## Cost Tracking

- Scan local session logs (JSONL) for token usage
- Apply built-in pricing table per model
- Aggregate 30-day rolling totals per provider
- Cache parsed results, re-parse only changed files (mtime check)

## Refresh Behavior

- Auto-poll at configurable interval (default 300s)
- `r` for immediate manual refresh
- `p` to pause/resume auto-polling
- `q` to quit
- Concurrent provider fetching via tokio

## Error Handling

- Provider fetch fails → show "unavailable" with last-known data
- No credentials → show "not configured" with setup hint
- Network timeout → stale data with age indicator

## Non-Goals (v1)

- Browser cookie scraping
- More than 3 providers
- Widget/menubar integration
- Interactive configuration editing
