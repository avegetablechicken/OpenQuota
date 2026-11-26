# Z.ai

OpenQuota tracks quota information for the Z.ai GLM Coding Plan.

## What it tracks

| Metric                           | Meaning                                                                    |
| -------------------------------- | -------------------------------------------------------------------------- |
| Session                          | Usage remaining in the rolling 5-hour window                               |
| Weekly                           | Usage remaining in the rolling 7-day window                                |
| Web Searches                     | Monthly web-search allowance remaining                                     |
| Today / Yesterday / Last 30 Days | Account-wide tokens reported by Z.ai and local-device estimated usage      |
| Usage Trend                      | 30-day usage sparkline (account and local-device scopes)                   |
| Credits                          | Exact credits consumed during the last 30 days for credit-based plans      |

## Setup

Add a Z.ai API key from **Customize** in OpenQuota. Saved keys are kept in the operating system's
credential store. OpenQuota also checks `ZAI_API_KEY`, `ZHIPUAI_API_KEY`, `GLM_API_KEY`,
`~/.config/openquota/zai.json`, and `~/.config/zai/key.json`; a key saved in the app takes priority.

The key must belong to an account with an active GLM Coding Plan.

OpenQuota reads account usage history with the same API key. Legacy token plans use Z.ai's model
usage history, while newer credit-based plans use the credit usage history with cached input,
uncached input, output-token, and model breakdowns. OpenQuota uses the `api.z.ai` service for quota
and account history. Credits remain a provider-specific unit and are never presented as US dollars.

## Local usage

Alongside account-wide history from `api.z.ai`, OpenQuota estimates local-device GLM usage from
ZCode (`~/.zcode/cli/db/db.sqlite` and `~/.zcode/v2/config.json`, or `ZCODE_HOME` when set) and
from other supported local coding clients on this device (`pi`, `oh-my-pi`, Antigravity, Claude
Code, Codex CLI, Grok CLI, Kimi Code, and OpenCode).

Local logs stay on your device and are never uploaded by OpenQuota. Only activity within the
trailing 30-day rolling window is aggregated; if ZCode's `~/.zcode` data directory was removed or
contains no usage in the last 30 days, local-device usage reflects only any remaining routed client
logs.

## Troubleshooting

- **Add an API key** — add a key in Customize or provide one through a supported external source.
- **API key invalid** — verify the key at [Z.ai API Keys](https://z.ai/manage-apikey/apikey-list).
- **No active coding plan** — confirm that the account has an active GLM Coding Plan.
- **No local history** — verify that `~/.zcode/cli/db/db.sqlite` (or `ZCODE_HOME`) or another
  supported local client has GLM activity within the last 30 days.
- **Usage unavailable** — check the connection and refresh again.
