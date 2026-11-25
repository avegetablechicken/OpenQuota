# Sub2API

Each Sub2API connection tracks either a user subscription or the upstream accounts managed by an administrator. Choose **Customize → Self-hostable providers → Add Account → Connection → Add**, then use **Track** to select a mode.

## User subscription

New connections default to **User subscription**. Choose the Codex or Claude configuration that supplies your Base URL, or enable **Custom Base URL** to enter the address manually. With a named provider, leave **API key** empty to use the key from that provider, or enter a key to override it. To remove a saved override, use the clear button in the key field and select **Save**. A custom Base URL requires a manually entered key.

For Codex, OpenQuota resolves the selected provider or standalone profile under `CODEX_HOME` (default `~/.codex`). It looks up the provider’s `env_key` in that directory’s `.env`, then in the process environment. Other supported sources in the selected provider are `experimental_bearer_token` and Bearer Authorization or X-API-Key headers in `env_http_headers` or `http_headers`. For Claude, OpenQuota reads `ANTHROPIC_AUTH_TOKEN` or `ANTHROPIC_API_KEY` from the selected settings file’s `env` object, then its top-level fields. Automatic mode reads the key again on refresh. If the provider endpoint changes, save the connection again.

For a subscription to an Antigravity or Grok group, choose that **Upstream**. These clients do not name their endpoint in a provider configuration, so enter the Base URL and API key; the card shows the chosen upstream's mark alongside Sub2API's.

With a Codex or Claude upstream, OpenQuota uses `GET /v1/models` to choose the subscription icon. A catalog that identifies one supported model family gets its mark alongside Sub2API’s. Mixed or unrecognized catalogs use the Sub2API mark.

OpenQuota reads `GET /v1/usage` using that key. The dashboard shows the plan name and quota percentages. The percentage appears below the bar. Unlimited daily and weekly windows show an empty bar with **No data** in the percentage position. The monthly row appears only when the server reports a positive monthly limit. The menu bar shows only used percentages for capped subscription windows. A weekly window also shows its next reset when the server supplies the window start. The **Today**, **Yesterday**, and **Last 30 Days** rows show token counts for this API key. The model breakdown ranks models by tokens. **Monthly** shows usage across the subscription since its current monthly window started. The window can reset partway through the last 30 days. For the key’s rolling 30-day cost, open **Total Spend**, select **Accounts**, **30 Days** and **Cost**. The subscription’s entry in Total Spend uses the daily fees reported for that key. Usage Trend plots the daily token totals for this API key relative to the largest daily total in view. Missing dates appear as zero, and an endpoint that returns only the today summary has no daily trend.

Wallet-only keys and keys with their own quota or rate limits can return a different view from this endpoint. If the response omits the subscription, OpenQuota displays a message explaining which key is needed. Manually entered keys are stored in the operating system credential store. Editing a saved subscription leaves the key field blank. Supply a new key when changing the endpoint. Each item remembers the connection and credentials for both tracking modes. Switching **Track** keeps the selected **Upstream**, including before the first save. Each mode has its own provider, Base URL and credentials. A new mode starts with an empty address, and switching back restores its draft. Select **Save** to activate it. **Clear connection** removes both saved connections.

## Upstream accounts (admin)

Choose **Upstream accounts (admin)** to see the active Codex, Claude, Antigravity or Grok OAuth accounts managed by the instance. Connections saved by earlier versions open in this mode.

### What it tracks

| Metric            | Meaning                                                 |
| ----------------- | ------------------------------------------------------- |
| Session           | Codex rolling session window, when reported             |
| Weekly            | Codex weekly window                                     |
| Spark             | Spark session window, when reported                     |
| Spark Weekly      | Spark weekly window, when reported                      |
| Rate Limit Resets | Available reset credits, shown as read-only information |
| Today, Yesterday, Last 30 Days | Token counts and cost for the upstream accounts |
| Model breakdown | Token counts and cost per model and account |

A Claude upstream shows Session, Weekly, and the Sonnet and Fable limits instead of the Codex rows.

An Antigravity upstream shows the same pools as the [Antigravity](antigravity.md) provider, read
from Sub2API's `antigravity_quota_groups`:

| Metric        | Meaning                                        |
| ------------- | ---------------------------------------------- |
| Session       | Gemini models' five-hour window                |
| Weekly        | Gemini models' weekly window                   |
| Claude        | Claude and GPT-OSS models' five-hour window    |
| Claude Weekly | Claude and GPT-OSS models' weekly window       |

Instances older than Sub2API 0.2.20 report only each model's window; the most used Gemini model
then stands for Session and the most used Claude or GPT-OSS model for Claude, without weekly rows.

A Grok upstream shows the xAI billing period Sub2API records for the account as **Weekly**, or as
**Monthly** when xAI reports a monthly period. Instances that have not yet probed the account's
billing, or that predate Sub2API 0.1.155, report no window.

An account that Sub2API marks as refused upstream or needing a new sign-in keeps its card with the
reason, and the other accounts still refresh.

An account's Session starts on first use, as does an Antigravity account's Claude window. When
the upstream reports it unused, the dashboard shows 0% used and **Not started** instead of a reset
countdown, and the menu bar omits the value.

OpenQuota reads each account's 30-day daily and model summaries from
`/api/v1/admin/accounts/:id/stats?days=30`. Upstream account costs use the
server's standard `cost` field, before user or account multipliers. Missing
standard costs remain unknown; OpenQuota never substitutes an adjusted
`actual_cost`. These are the instance's recorded prices, not an independent
recalculation using OpenQuota's historical price catalog. Daily buckets follow
the server's calendar, and model names follow its summary.

Both tracking modes use summaries rather than individual request records.
OpenQuota does not call the administrator request-log endpoint, download pages
of historical requests, or read the old detail cache. Existing detail cache
files are left untouched. The normal snapshot cache retains the last successful
summary if a refresh fails.

Quota is shown first. The refresh icon keeps spinning while summaries are
loading, then stops when the refresh completes or fails. Failures show an error.
Each account keeps its own summary, and the connection total combines the active
accounts. Requests use gzip when supported by the server.

### Administrator setup

In **Customize**, select **Add Account** under **Self-hostable providers** to create an independent account. In its **Connection**
card, select **Add**, choose **Upstream accounts (admin)**, and configure the instance address and administrator login. Select **Save** to
collapse the form, or **Done** to discard edits. Use **Edit** to reopen a configured connection. OpenQuota signs in through
`/api/v1/auth/login`, discovers the active OAuth accounts for the selected upstream, and reads Codex quota from
`/api/v1/admin/openai/accounts/:id/quota`, or Claude, Antigravity and Grok quota from
`/api/v1/admin/accounts/:id/usage`.

Antigravity and Grok clients do not name their endpoint in a provider configuration. Selecting
either upstream keeps an address already read from a Codex or Claude **Provider**; otherwise
enter the instance address, such as `https://sub2api.example.com`, as the Base URL.

Changing **Upstream** changes which accounts OpenQuota tracks on the instance. It keeps the current address, login and named provider configuration. A saved password is reused only for the same instance. To choose another named provider using the newly selected platform’s configuration, clear the Provider field and enter the new name.

For a Codex upstream, enter a **Provider** that exactly matches the top-level `model_provider` value,
a name under `[model_providers]` in `~/.codex/config.toml`, or a standalone profile file named
`~/.codex/<profile>.config.toml`. OpenQuota reads that configuration's model provider and fills the
Base URL from `[model_providers.<name>].base_url`; the editor shows that URL under Provider.
Enable **Custom Base URL** to enter an endpoint manually. Custom Base URL mode clears Provider and
puts an editable Base URL in its place, so the two sources cannot be combined.

For a Claude upstream, enter a **Provider** name. OpenQuota searches `$CLAUDE_CONFIG_DIR`
(default `~/.claude`) and its subdirectories, case-sensitively, without following symlinks.
First, it looks for JSON filenames containing both `settings` and the entered name: for example,
`work` matches `work.settings.json` or `settings-work.json`. Only when there are no such matches
will it match an exact JSON filename stem: `api` matches `profiles/api.json`. Full filenames such
as `api.json` also work, but use the same settings-first priority. Enter a name, not a path.
Multiple matches at either stage produce an error instead of choosing one or falling back.

The selected file's `env.ANTHROPIC_BASE_URL` supplies the endpoint, with a top-level
`ANTHROPIC_BASE_URL` accepted only when `env.ANTHROPIC_BASE_URL` is absent. An empty value is an error;
OpenQuota does not fall back to another file or the environment. The resolved Base URL is shown
under Provider and is resolved again when saving. Enable **Custom Base URL** to enter it manually instead.
Administrator email and password must still be entered separately. Existing Claude connections
without a Provider name show in custom-address mode with their saved URL preserved.

Each complete connection is stored under its own entry in the operating system's credential store.
The connection is stored and confirmed as soon as **Save** is selected, without first authenticating
it or fetching an access token. OpenQuota refreshes its quota in the background once it is saved.
The password is not returned to the interface after it is saved, and login or quota response bodies
are not written to OpenQuota logs. The small clear control beside **Save** removes only the
Connection fields and keeps the Sub2API item. The **Delete Sub2API** row at the bottom removes the
whole item, including an empty item whose login has not been saved yet.

Each of the eight Sub2API slots can track multiple active accounts for its selected upstream. Each
account has its own quota and usage history. The connection total includes all of those accounts.

## Troubleshooting

HTTPS requests use native TLS verification: Security Framework on macOS, SChannel on
Windows, and OpenSSL on Linux. A private or self-signed certificate must be trusted
on each machine; the certificate must also be valid for the configured host and date.
On Ubuntu, install a trusted PEM CA certificate as a `.crt` file under
`/usr/local/share/ca-certificates/`, run `sudo update-ca-certificates`, and restart
OpenQuota. A locally trusted self-signed server certificate with `CA:TRUE` is supported
through native verification. OpenQuota does not disable certificate or hostname checks.

- **Relay requires a Bearer API token** — select **Custom Base URL** in administrator mode and enter the instance’s own address. The subscription mode can keep using the relay.
- **Login rejected** — verify the administrator email and password in the Sub2API admin panel.
- **Administrator access required** — the configured login must be a Sub2API administrator.
- **Two-factor authentication required** — this initial integration does not complete a 2FA login.
- **No upstream accounts** — add or enable an OAuth account of the selected platform in Sub2API.
  Antigravity accounts are listed under the `antigravity` platform, separately from Gemini CLI's.
- **Could not reach Sub2API** — verify the Base URL, TLS certificate, proxy, and network connection.
