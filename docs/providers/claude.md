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

## Switching accounts

OpenQuota shows one Claude card for the login in the active Claude Code configuration directory.
Signing in to a different account there updates the same card without restarting OpenQuota: its
layout and proxy stay in place, while the card name and cached usage follow the active account.

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
process environment. The settings file is `$CLAUDE_CONFIG_DIR/settings.json`, or
`~/.claude/settings.json`.
Explicit proxy or direct-routing preferences in OpenQuota still take precedence.
Explicit proxies bypass loopback destinations (`localhost`, `127.0.0.0/8`, and `::1`).

Subscription queries and token refreshes follow Claude Code's OAuth environment variables:
`CLAUDE_CODE_CUSTOM_OAUTH_URL`, `CLAUDE_CODE_OAUTH_CLIENT_ID`, and the `USER_TYPE=ant` local or
staging switches (`USE_LOCAL_OAUTH`, `CLAUDE_LOCAL_OAUTH_API_BASE`, `USE_STAGING_OAUTH`). On macOS,
the matching Claude Code keychain namespace is used, including for `CLAUDE_CONFIG_DIR`. These
variables come from the process environment; on macOS and Linux, all except
`CLAUDE_CODE_OAUTH_CLIENT_ID` also fall back to the login-shell snapshot saved by a previous
launch. They are not read from `settings.json`.
Only the proxy fields listed above are returned by the settings reader.
Tokens in `settings.json.env` are not imported;
process-environment OAuth tokens retain their inference-only behavior.
These lookups do not export variables or affect Codex or other providers.
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

Claude quota countdowns and pacing use the usage response's HTTP `Date` header to
correct local clock skew, including virtual machines with a fast or slow clock.
Reset timestamps remain UTC instants; exact times are displayed in the local time
zone. The offset is cached with each quota. Old caches and responses without a
valid `Date` header use the local clock until a successful refresh supplies one.
