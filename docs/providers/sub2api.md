# Sub2API

OpenQuota can show Codex quota from a self-hosted Sub2API instance as a separate provider. The
normal Codex provider and its authentication flow are unchanged.

## What it tracks

| Metric            | Meaning                                                 |
| ----------------- | ------------------------------------------------------- |
| Session           | Codex rolling session window, when reported             |
| Weekly            | Codex weekly window                                     |
| Spark             | Spark session window, when reported                     |
| Spark Weekly      | Spark weekly window, when reported                      |
| Rate Limit Resets | Available reset credits, shown as read-only information |

## Setup

Open **Customize**, select **Sub2API**, and add the instance Base URL, administrator email, and
password. OpenQuota signs in through `/api/v1/auth/login`, discovers active OpenAI OAuth accounts,
and reads quota from `/api/v1/admin/openai/accounts/:id/quota`.

The complete connection is stored in the operating system's credential store. The password is not
returned to the interface after it is saved, and login or quota response bodies are not written to
OpenQuota logs.

This initial integration registers one Sub2API provider and supports Codex upstream accounts only.
If the instance contains multiple active Codex accounts, OpenQuota shows the first account sorted by
name and adds a warning to the provider card identifying that account.

## Troubleshooting

- **Login rejected** — verify the administrator email and password in the Sub2API admin panel.
- **Administrator access required** — the configured login must be a Sub2API administrator.
- **Two-factor authentication required** — this initial integration does not complete a 2FA login.
- **No Codex upstream found** — add or enable an OpenAI OAuth account in Sub2API.
- **Could not reach Sub2API** — verify the Base URL, TLS certificate, proxy, and network connection.
