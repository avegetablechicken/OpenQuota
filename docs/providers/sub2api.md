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

For a Codex upstream, enter a **Provider** that exactly matches the top-level `model_provider` value,
a name under `[model_providers]` in `~/.codex/config.toml`, or a standalone profile file named
`~/.codex/<profile>.config.toml`. OpenQuota reads that configuration's model provider and fills the
Base URL from `[model_providers.<name>].base_url`; that URL is read-only in the connection editor.
Enable **Custom Base URL** to enter an endpoint manually. Custom Base URL mode clears and disables
Provider, so the two sources cannot be combined.

For a Claude upstream, OpenQuota first reads `ANTHROPIC_BASE_URL` from
`~/.claude/settings.json` (including its `env` object), then falls back to the environment. The
resolved Base URL is read-only by default. Enable **Custom Base URL** to replace it with a manually
entered endpoint.

Each complete connection is stored under its own entry in the operating system's credential store.
The connection is stored and confirmed as soon as **Save** is selected, without first authenticating
it or fetching an access token. OpenQuota refreshes its quota in the background after the editor
closes. The password is not returned to the interface after it is saved, and login or quota response
bodies are not written to OpenQuota logs. The small clear control beside **Save** removes only the
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
