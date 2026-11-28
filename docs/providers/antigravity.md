# Antigravity

OpenQuota tracks the shared Gemini and Claude quota pools reported by Antigravity, along with
local token usage and API-equivalent spend estimated from Antigravity's local session transcripts.

## What it tracks

| Metric        | Meaning                                                      |
| ------------- | ------------------------------------------------------------ |
| Session       | Shared Gemini pool in the rolling 5-hour window              |
| Weekly        | Shared Gemini pool in the weekly window                      |
| Claude        | Shared non-Gemini pool in the rolling 5-hour window          |
| Claude Weekly | Shared non-Gemini pool in the weekly window                  |
| Trend         | 30-day local usage sparkline from Antigravity transcripts    |
| Today         | Local tokens and estimated spend recorded today              |
| Yesterday     | Local tokens and estimated spend recorded yesterday          |
| Last 30 Days  | Local tokens and estimated spend across the trailing 30 days |

Only the pools available to the signed-in account are shown.

The Session and Claude five-hour pools start on first use. Until then the dashboard shows 0% used
and **Not started** instead of a reset countdown, and the menu bar omits the value.

## Sign-in and local data

Sign in through Antigravity or run `agy`. OpenQuota reuses Antigravity's locally stored credentials
and can also discover a compatible running Antigravity language server. No API key needs to be added
to OpenQuota.

Local usage is read from Antigravity's conversation transcripts under `~/.gemini/antigravity/brain/`
and `~/.gemini/antigravity-cli/brain/` (or `ANTIGRAVITY_HOME` / `ANTIGRAVITY_DATA_DIR` when set),
Gemini CLI session logs under `~/.gemini/tmp/*/chats/` (or `GEMINI_CLI_HOME` when set, skipping
manual checkpoints and deduplicating resumed sessions by message ID), and routed Gemini model usage
recorded by other supported local coding clients (`pi`, `oh-my-pi`, Claude Code, Codex CLI, Grok
CLI, Kimi Code, OpenCode, and ZCode). Models without a known API rate still appear in token counts
and model breakdowns with a missing-price indicator.

When the stored access token is rejected, OpenQuota first rereads the shared sign-in in case
Antigravity or `agy` renewed it. Access tokens OpenQuota renews itself are saved back to the shared
sign-in when possible, unless it now holds a different account or a newer token, and are also kept
in OpenQuota's own data folder.

On macOS, OpenQuota reads and writes the shared sign-in (Keychain item `gemini`, account
`antigravity`) through `/usr/bin/security`, as `agy` does. Every write sets the item's partition
list to the writer's, so writing through the same tool keeps it at `apple-tool:` and Antigravity,
`agy` and OpenQuota keep access without new Keychain prompts. If another app last wrote the item
directly, macOS may ask once to allow `security` access until the item is next saved this way. A
request left unanswered for a minute is abandoned and retried on a later refresh.

## Troubleshooting

- **Not signed in** — open Antigravity or run `agy`, complete sign-in, then refresh OpenQuota.
- **Sign-in expired** — authenticate again in Antigravity or with `agy`.
- **Quota permission denied (HTTP 403)** — Google rejected the quota request. `SUBSCRIPTION_REQUIRED` means Google did not recognize a valid license for that request. OpenQuota shows the reason and numeric reference when available.
- **Credentials could not be read** — reopen Antigravity so its local sign-in state can be restored.
- **Usage temporarily unavailable** — wait briefly and refresh again.

Cloud quota requests use the Antigravity `daily-cloudcode-pa.googleapis.com` environment.
The other Cloud Code environment can report untouched quota pools for the same login and is
not a fallback. A transient request failure keeps the last successful quota snapshot; only an
explicitly unsupported API allows trying an older quota API on the same environment.
