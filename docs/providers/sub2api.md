# Sub2API

OpenQuota can show Codex quota from self-hosted Sub2API instances as separate providers. The normal
Codex provider and its authentication flow are unchanged.

## What it tracks

| Metric            | Meaning                                                 |
| ----------------- | ------------------------------------------------------- |
| Session           | Codex rolling session window, when reported             |
| Weekly            | Codex weekly window                                     |
| Spark             | Spark session window, when reported                     |
| Spark Weekly      | Spark weekly window, when reported                      |
| Rate Limit Resets | Available reset credits, shown as read-only information |

## Setup

Open **Customize**, select **+ Sub2API**, and add the instance Base URL, administrator email, and
password. Each click creates the next independent Sub2API item. OpenQuota signs in through
`/api/v1/auth/login`, discovers active OpenAI OAuth accounts, and reads quota from
`/api/v1/admin/openai/accounts/:id/quota`.

Each complete connection is stored under its own entry in the operating system's credential store.
The password is not returned to the interface after it is saved, and login or quota response bodies
are not written to OpenQuota logs. The small clear control beside **Save** removes only the
Connection fields and keeps the Sub2API item. The **Delete Sub2API** row at the bottom removes the
whole item, including an empty item whose login has not been saved yet.

This integration currently supports up to eight independent Sub2API items and Codex upstream
accounts only. If one instance contains multiple active Codex accounts, OpenQuota shows the first
account sorted by name and adds a warning to the provider card identifying that account. A valid
administrator login can be saved even when the instance has no active Codex account.

## Troubleshooting

- **Login rejected** — verify the administrator email and password in the Sub2API admin panel.
- **Administrator access required** — the configured login must be a Sub2API administrator.
- **Two-factor authentication required** — this initial integration does not complete a 2FA login.
- **No Codex upstream found** — add or enable an OpenAI OAuth account in Sub2API.
- **Could not reach Sub2API** — verify the Base URL, TLS certificate, proxy, and network connection.
