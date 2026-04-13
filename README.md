# claudetop

Terminal dashboard for monitoring AI provider usage and costs.

```
 ┌ ClaudeTop - AI Provider Dashboard ─────────────────┐
 │                                                     │
 │  ┌─ Claude ──────────────────────────────────────┐  │
 │  │ Status: OK   Plan: Pro   Cost (30d): $42.15   │  │
 │  │ ██████████████████░░░░░░░░░░  Daily: 62%      │  │
 │  └───────────────────────────────────────────────┘  │
 │  ┌─ Codex ───────────────────────────────────────┐  │
 │  │ Status: OK   Plan: Pro   Cost (30d): $18.30   │  │
 │  │ ████████░░░░░░░░░░░░░░░░░░░░  Daily: 28%     │  │
 │  └───────────────────────────────────────────────┘  │
 │  ┌─ Gemini ──────────────────────────────────────┐  │
 │  │ Status: OK   Plan: Free  Cost (30d): $0.00    │  │
 │  │ ██░░░░░░░░░░░░░░░░░░░░░░░░░░  Daily: 5%      │  │
 │  └───────────────────────────────────────────────┘  │
 │                                                     │
 │  [r] refresh  [p] pause  [q/Esc] quit               │
 └─────────────────────────────────────────────────────┘
```

## Features

- Real-time usage monitoring for Claude, Codex, and Gemini
- Rate limit tracking with visual gauges
- 30-day cost estimation from local session logs
- Auto-refresh with configurable interval
- Zero-config — works out of the box using local credentials

## Install

```bash
cargo install claudetop
```

Or build from source:

```bash
git clone https://github.com/randomradio/claudetop.git
cd claudetop
cargo install --path .
```

## Usage

```bash
claudetop
```

### Keybindings

| Key       | Action           |
|-----------|------------------|
| `r`       | Refresh now      |
| `p`       | Pause/resume     |
| `q`/`Esc` | Quit             |
| `Ctrl+c`  | Quit             |

## Configuration

Optional. claudetop works with defaults, but you can customize via:

```
~/.config/claudetop/config.toml
```

```toml
[general]
refresh_interval_secs = 300  # default: 5 minutes

[claude]
enabled = true

[codex]
enabled = true

[gemini]
enabled = true
```

### Authentication

claudetop reads credentials from the same locations as each provider's CLI tool:

- **Claude**: Reads from the local Claude Code keychain (`~/.claude/`)
- **Codex**: Reads from the local Codex configuration
- **Gemini**: Uses API key from config or environment

### Cost Estimation

Costs are estimated by scanning local JSONL session logs from Claude Code and Codex. Token counts are priced using published per-model rates. This is an estimate — check your provider dashboard for exact billing.

## License

[MIT](LICENSE)
