# Claude Code

OpenQuota tracks Claude subscription limits and local Claude usage history.

## What it tracks

| Metric                           | Meaning                                                      |
| -------------------------------- | ------------------------------------------------------------ |
| Session                          | Usage remaining in the current session window                |
| Weekly                           | Usage remaining in the weekly window                         |
| Sonnet / Fable                   | Model-specific limits when they are reported for the account |
| Extra Usage                      | Extra-usage allowance or spending reported by Claude         |
| Today / Yesterday / Last 30 Days | Tokens and estimated spend calculated from local usage logs  |
| Usage Trend                      | Recent local usage over time                                 |

## Sign-in and local data

Sign in with Claude Code by running `claude`. OpenQuota reuses the credentials maintained by the
CLI, including `CLAUDE_CONFIG_DIR` when it is set. Refreshed CLI credentials are saved back to the
same source when possible.

## Multiple accounts

OpenQuota discovers separate Claude Code logins that use custom `CLAUDE_CONFIG_DIR` homes and shows
each account as its own card with independent limits, plan, and local usage history. Logins belonging
to the same Claude account are combined automatically.

Account cards can be renamed from Customize or from the dashboard. If a login is removed, its card
is hidden and returns with its previous customization when the login is detected again.

Live subscription limits currently require a Claude Code login. On macOS, OpenQuota can recognize
that Claude Desktop is installed, but it does not reuse Desktop's encrypted session. Run `claude`
and sign in once if Desktop is your only Claude login.

Spend history is calculated locally from Claude usage logs. It can also include compatible Claude
usage recorded by pi (`~/.pi/agent/sessions`) or oh-my-pi (`~/.omp/agent/sessions`), Anthropic usage
recorded by local OpenCode sessions, and, on macOS, Claude's local agent-mode sessions. These local
records are not uploaded by OpenQuota.

## Environment settings

Claude requests read proxy variables (`HTTP_PROXY`, `HTTPS_PROXY`, `ALL_PROXY`, `NO_PROXY`,
including lowercase aliases) from the account's `settings.json` `env` object before checking the
process environment. The default account uses `$CLAUDE_CONFIG_DIR/settings.json`, or
`~/.claude/settings.json`; discovered accounts use their own configuration directory.
Explicit proxy or direct-routing preferences in OpenQuota still take precedence.

Claude OAuth environment lookups also consult the account's settings, preserving the existing
rules for stored login credentials and inference-only tokens. These lookups do not export variables or affect Codex or other providers.
Cached HTTP clients may require restarting OpenQuota after proxy changes.

OpenQuota does not currently track Claude launch arguments, so arbitrary `--settings` paths,
inline settings, and project-specific settings are not automatically selected. API keys do not
replace the Claude login required for subscription-limit queries.

## Troubleshooting

- **Not logged in** — run `claude`, complete sign-in, then refresh OpenQuota.
- **Claude Desktop login found** — sign in once through the Claude Code CLI.
- **Session or token expired** — sign in again with `claude`.
- **No local history** — use Claude Code normally and check whether `CLAUDE_CONFIG_DIR` points to
  the directory containing your Claude data.
