# Antigravity

OpenQuota tracks the shared Gemini and Claude quota pools reported by Antigravity.

## What it tracks

| Metric        | Meaning                                             |
| ------------- | --------------------------------------------------- |
| Session       | Shared Gemini pool in the rolling 5-hour window     |
| Weekly        | Shared Gemini pool in the weekly window             |
| Claude        | Shared non-Gemini pool in the rolling 5-hour window |
| Claude Weekly | Shared non-Gemini pool in the weekly window         |

Only the pools available to the signed-in account are shown.

## Sign-in and local data

Sign in through Antigravity or run `agy`. OpenQuota reuses Antigravity's locally stored credentials
and can also discover a compatible running Antigravity language server. No API key needs to be added
to OpenQuota.

When the stored access token is rejected, OpenQuota first rereads the shared sign-in in case
Antigravity or `agy` renewed it. Access tokens OpenQuota renews itself are saved back to the shared
sign-in when possible, unless it now holds a different account or a newer token, and are also kept
in OpenQuota's own data folder.

## Troubleshooting

- **Not signed in** — open Antigravity or run `agy`, complete sign-in, then refresh OpenQuota.
- **Sign-in expired** — authenticate again in Antigravity or with `agy`.
- **Credentials could not be read** — reopen Antigravity so its local sign-in state can be restored.
- **Usage temporarily unavailable** — wait briefly and refresh again.
