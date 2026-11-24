# Sub2API

OpenQuota shows quota and upstream usage estimates for active Codex or Claude OAuth accounts on self-hosted Sub2API instances.

## What it tracks

| Metric            | Meaning                                                 |
| ----------------- | ------------------------------------------------------- |
| Session           | Codex rolling session window, when reported             |
| Weekly            | Codex weekly window                                     |
| Spark             | Spark session window, when reported                     |
| Spark Weekly      | Spark weekly window, when reported                      |
| Rate Limit Resets | Available reset credits, shown as read-only information |
| Today, Yesterday, Last 30 Days | Token counts and cost using the selected billing method |
| Model breakdown | Token counts and cost per model and account |

An account's Session starts on first use. When the upstream reports it unused, the dashboard shows
0% used and **Not started** instead of a reset countdown, and the menu bar omits the value.

The last field in the connection editor is **Billing method**. **Upstream** is the default,
including for existing connections. **Sub2API** reads the instance's 30-day account statistics
from `/api/v1/admin/accounts/:id/stats`. It uses the account's reported `actual_cost`, including
zero, or `cost` when the actual amount is unavailable. Days with usage but no amount make the
period cost unknown. The daily buckets follow the server's calendar. Saving a change refreshes
the account and connection totals.

With **Upstream**, OpenQuota reads paginated request records from `/api/v1/admin/usage` for each account. It uses the
model reported by the upstream response, then the model forwarded upstream, then the request model
when no mapping is recorded. Each estimate uses the request timestamp, speed tier, and input,
output, cache-read and cache-write tokens. An unknown rate or unsupported image charge leaves an
incomplete estimate. The 30-day window includes today and the previous 29 local dates.
Instance markups and account multipliers apply separately to the instance's own billing.
See [historical pricing](../pricing-history.md).

OpenQuota keeps a local history cache for each connection and upstream account. Switching
billing methods or restarting the app reuses the data already synced for each method.
Quota and account discovery still refresh normally.

Each refresh reads every account's quota first and shows it right away. Usage history
loads afterwards, and the previous history stays on screen until it finishes, so a slow
or busy instance never delays the quota. If loading history fails, the new quota is
kept and the error is shown on the card.

- **Upstream** stores the fields needed for pricing. A newly discovered account
  downloads only today's request records, then later syncs request new records after
  the saved position. Days before that are shown with the instance's own token counts
  and cost from `/api/v1/admin/accounts/:id/stats`, under the model row
  **Before sync (Sub2API cost)**, read once per day. Coverage grows by one day each
  day until it spans the 30-day window. Accounts that an earlier version had already
  synced completely keep their full window.
- To price the whole window now, select **Backfill 30 Days** under **Usage history** in
  the connection editor. This downloads every request record of the past 30 days, which
  can be a lot of data on a busy instance, so it never starts on its own. Each refresh
  downloads up to 16 pages of 128 records and saves each complete page; the earlier
  days stay on the server summary until the backfill completes. If the server removes
  records, OpenQuota checks the affected dates before rebuilding them.
- **Sub2API** stores the server's summary and updates the current day's contribution
  when usage changes. It refreshes the 30-day summary when the calendar window
  changes or when the daily and monthly figures cannot be reconciled.
- When the server reports the same account activity timestamp, routine refreshes reuse
  the history cache. OpenQuota periodically checks for delayed changes and uses a
  lightweight history check on instances that omit the timestamp.

## Setup

In **Customize**, select **+ Sub2API** to create an independent account. In its **Connection**
card, select **Add** to configure the instance address and administrator login. Select **Save** to
collapse the form, or **Done** to discard edits. Use **Edit** to reopen a configured connection. OpenQuota signs in through
`/api/v1/auth/login`, discovers the active OAuth accounts for the selected upstream, and reads Codex quota from
`/api/v1/admin/openai/accounts/:id/quota` or Claude quota from `/api/v1/admin/accounts/:id/usage`.

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

- **Login rejected** — verify the administrator email and password in the Sub2API admin panel.
- **Administrator access required** — the configured login must be a Sub2API administrator.
- **Two-factor authentication required** — this initial integration does not complete a 2FA login.
- **No Codex upstream found** — add or enable an OpenAI OAuth account in Sub2API.
- **Could not reach Sub2API** — verify the Base URL, TLS certificate, proxy, and network connection.
