# Z.ai

OpenQuota tracks quota information for the Z.ai GLM Coding Plan.

## What it tracks

| Metric       | Meaning                                      |
| ------------ | -------------------------------------------- |
| Session      | Usage remaining in the rolling 5-hour window |
| Weekly       | Usage remaining in the rolling 7-day window  |
| Web Searches | Monthly web-search allowance remaining       |
| Today / Yesterday / Last 30 Days | Account-wide model tokens reported by Z.ai |
| Credits      | Exact credits consumed during the last 30 days for credit-based plans |

## Setup

Add a Z.ai API key from **Customize** in OpenQuota. Saved keys are kept in the operating system's
credential store. OpenQuota also checks `ZAI_API_KEY`, `GLM_API_KEY`,
`~/.config/openquota/zai.json`, and `~/.config/zai/key.json`; a key saved in the app takes priority.

The key must belong to an account with an active GLM Coding Plan.

OpenQuota reads account usage history with the same API key. Legacy token plans use Z.ai's model
usage history, while newer credit-based plans use the credit usage history with cached input,
uncached input, output-token, and model breakdowns. If the international endpoint has no history,
OpenQuota checks the BigModel endpoint without combining the two sources, so mirrored data is not
counted twice. Credits remain a provider-specific unit and are never presented as US dollars.

## Troubleshooting

- **Add an API key** — add a key in Customize or provide one through a supported external source.
- **API key invalid** — verify the key at [Z.ai API Keys](https://z.ai/manage-apikey/apikey-list).
- **No active coding plan** — confirm that the account has an active GLM Coding Plan.
- **Usage unavailable** — check the connection and refresh again.
