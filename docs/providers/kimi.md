# Kimi

OpenQuota tracks the Session (rolling five-hour) and Weekly quotas of a Kimi Code membership, along
with local-device token usage and API-equivalent spend estimated from local logs.

## What it tracks

| Metric                           | Meaning                                                       |
| -------------------------------- | ------------------------------------------------------------- |
| Session                          | Usage remaining in the rolling 5-hour window                  |
| Weekly                           | Usage remaining in the rolling 7-day window                   |
| Today / Yesterday / Last 30 Days | Local tokens and estimated spend across the trailing 30 days  |
| Usage Trend                      | 30-day local usage sparkline                                  |

See [historical API rates and cost calculations](../pricing-history.md) for dated prices and
coverage.

## Setup

Create a Kimi Code API key in the [Kimi Code Console](https://www.kimi.com/code/console), then add
it in **Customize** in OpenQuota. Saved keys are stored in the operating system's credential store.
OpenQuota also checks `KIMI_API_KEY` and `~/.config/openquota/kimi.json`; a key saved in the app
takes priority.

This provider uses the Kimi Code endpoint, `https://api.kimi.com/coding/v1`. Kimi Code keys are not
interchangeable with Kimi Open Platform keys.

## Local usage

OpenQuota reads local Kimi usage from Kimi Code session logs (`~/.kimi-code/sessions/**/wire.jsonl`,
or `KIMI_CODE_HOME` when set) and legacy Kimi CLI logs (`~/.kimi/sessions/**/wire.jsonl`, or
`KIMI_SHARE_DIR` when set), deduplicating sessions that were migrated between the two directories.
It also folds Kimi and Moonshot model usage recorded by other supported local coding clients on this
device (`pi`, `oh-my-pi`, Antigravity, Claude Code, Codex CLI, Grok CLI, OpenCode, and ZCode).

Local logs stay on your device and are never uploaded by OpenQuota. Only activity within the
trailing 30-day rolling window is aggregated; models without a known API rate remain visible in
token counts with a missing-price indicator.

## Troubleshooting

- **Add an API key** — add a Kimi Code key in Customize or provide it through a supported external source.
- **API key invalid** — create or verify the key in the [Kimi Code Console](https://www.kimi.com/code/console).
- **No local history** — confirm recent activity exists within the last 30 days under `~/.kimi-code`
  (or `KIMI_CODE_HOME`), `~/.kimi` (or `KIMI_SHARE_DIR`), or another supported local client.
- **Usage unavailable** — check the connection and refresh again.
