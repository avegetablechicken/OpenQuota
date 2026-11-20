# 历史定价与费用估算

核对日期为 2026-11-20。下表的单位是美元 / 百万 token。OpenQuota 用请求发生时的模型、计费渠道和费率估算 API 等价费用。订阅内使用同样的 token 数，实际追加扣款可能为零。日志或服务端提供金额时，应优先看对应账单。逐模型费率、时间边界、缓存写入价和来源见 [机器可读费率表的 history 字段](../src-tauri/resources/pricing_supplement.json)。

OpenQuota 启动时检查本地费率缓存，运行期间每个来源距上次成功检查满 24 小时后自动同步。即使关闭所有 provider，后台也会检查 LiteLLM、models.dev 和 OpenQuota 维护的历史费率表。同步成功后，下次刷新用量时会使用更新后的费率。某个来源暂时不可用时，继续使用已有费率，30 分钟后重试该来源。

## Codex / OpenAI

以下为标准、短上下文文本请求。每行日期是该价格的起点，同模型的下一行取代上一行。模型不同的行可以同时有效。

| 起始日期 | 模型 | 普通输入 | 缓存读取 | 输出 | 官方依据 |
|---|---|---:|---:|---:|---|
| 2025-04-16 → 06-10 | o3 | 10 → 2 | 2.50 → 0.50 | 40 → 8 | [发布公告](https://openai.com/index/introducing-o3-and-o4-mini/) · [降价公告](https://community.openai.com/t/o3-is-80-cheaper-and-introducing-o3-pro/1284925) |
| 2025-05-16 | codex-mini-latest | 1.50 | 0.375 | 6 | [Codex 发布公告](https://openai.com/index/introducing-codex/) |
| 2025-09-15 | GPT-5-Codex | 1.25 | 0.125 | 10 | [模型价目表](https://developers.openai.com/api/docs/models/gpt-5-codex) |
| 2025-11-12 | GPT-5.1-Codex / Mini | 1.25 / 0.25 | 0.125 / 0.025 | 10 / 2 | [Codex](https://developers.openai.com/api/docs/models/gpt-5.1-codex) · [Mini](https://developers.openai.com/api/docs/models/gpt-5.1-codex-mini) |
| 2025-12-18 / 2026-02-05 | GPT-5.2-Codex / GPT-5.3-Codex | 1.75 | 0.175 | 14 | [5.2](https://developers.openai.com/api/docs/models/gpt-5.2-codex) · [5.3](https://developers.openai.com/api/docs/models/gpt-5.3-codex) |
| 2026-03-05 | GPT-5.4 | 2.50 | 0.25 | 15 | [模型价目表](https://developers.openai.com/api/docs/models/gpt-5.4) |
| 2026-04-23 | GPT-5.5 | 5 | 0.50 | 30 | [模型价目表](https://developers.openai.com/api/docs/models/gpt-5.5) |
| 2026-06-26 | GPT-5.6 Sol / Terra / Luna 预览 | 5 / 2.50 / 1 | 0.50 / 0.25 / 0.10 | 30 / 15 / 6 | [预览发布价](https://openai.com/index/previewing-gpt-5-6-sol/) |
| 2026-07-30 | GPT-5.6 Terra / Luna | 2 / 0.20 | 0.20 / 0.02 | 12 / 1.20 | [首次调价](https://openai.com/index/advancing-the-price-performance-frontier-with-gpt-5-6/) |
| 2026-08-21 至 11-30 | GPT-5.6 Sol | 4 | 0.40 | 20 | [调价公告](https://community.openai.com/t/20-price-reduction-for-gpt-5-6-sol-api-codex-credits-and-chatgpt-work/1391726) · [优惠期限](https://developers.openai.com/api/docs/models/gpt-5.6-sol) |
| 2026-09-03 | GPT-6 Astra | 10 | 1 | 50 | [模型发布公告](https://openai.com/index/gpt-6-astra/) |
| 2026-09-22 | GPT-6 Sol / Luna | 2 / 0.10 | 0.20 / 0.01 | 10 / 0.50 | [发布公告](https://openai.com/index/introducing-gpt-6-sol-and-luna/) |
| 2026-09-29 / 10-26 | GPT-6.1 Sol / Terra | 2 / 1 | 0.10 / 0.05 | 10 / 5 | [API 变更记录](https://developers.openai.com/api/docs/changelog) |
| 2026-11-17 | GPT-6.2 Astra / Sol | 8 / 2 | 0.40 / 0.10 | 40 / 10 | [发布公告](https://openai.com/index/introducing-gpt-6-2-astra-sol/) |

GPT-6.2 单次请求输入超过 272,000 token 时，整次请求的普通输入和缓存单价乘 2，输出单价乘 1.5。Fast 为标准价的 2 倍。Ultrafast 在 2027-02-28 太平洋时间结束前为 3 倍，随后为 6 倍。GPT-5.6 及以后的显式缓存写入按保留期计价，30 分钟为输入价的 1.25 倍，12 小时为 2.5 倍。旧日志只有写入总数时采用默认保留期。输出中的推理 token 已计入输出。 [GPT-6.2 模型说明](https://developers.openai.com/api/docs/models/gpt-6.2-astra) · [提示缓存](https://developers.openai.com/api/docs/guides/prompt-caching)

自 2026-10-06 起，ChatGPT 账号的 Codex 自动审批免费。OpenQuota 对原生日志同时核对 OpenAI 渠道和该条用量记录的 ChatGPT 方案。缺少方案、来自其他渠道或早于该日的自动审批保留 token 数，金额记为未知。 [OpenAI 方案说明](https://help.openai.com/en/articles/11369540-using-codex-with-your-chatgpt-plan) · [10 月 6 日公告](https://community.openai.com/t/28-days-of-quality-of-life-improvements-and-or-a-full-reset/1403525/17)

## Claude Code / Anthropic

| 起始日期 | 模型或条件 | 普通输入 | 缓存读取 | 输出 | 官方依据 |
|---|---|---:|---:|---:|---|
| 2024-12-03 | Haiku 3.5 调价 | 0.80 | 0.08 | 4 | [公告更新](https://www.anthropic.com/news/3-5-models-and-computer-use) |
| 2025-05-22 | Sonnet 4 / Opus 4 | 3 / 15 | 0.30 / 1.50 | 15 / 75 | [Claude 4](https://www.anthropic.com/news/claude-4) |
| 2025-10-15 / 11-24 | Haiku 4.5 / Opus 4.5 | 1 / 5 | 0.10 / 0.50 | 5 / 25 | [平台历史记录](https://platform.claude.com/docs/en/release-notes/overview) |
| 2026-02-05 / 02-17 | Opus 4.6 / Sonnet 4.6，输入 ≤200k | 5 / 3 | 0.50 / 0.30 | 25 / 15 | [Opus 4.6](https://www.anthropic.com/news/claude-opus-4-6) · [Sonnet 4.6](https://www.anthropic.com/news/claude-sonnet-4-6) |
| 同上，至 2026-03-12 | 同模型，输入 >200k | 10 / 6 | 1 / 0.60 | 37.50 / 22.50 | [长上下文变更公告](https://claude.com/resources/articles/1m-context-ga) |
| 2026-03-13 | 同模型，含完整 1M 上下文 | 5 / 3 | 0.50 / 0.30 | 25 / 15 | [平台历史记录](https://platform.claude.com/docs/en/release-notes/overview) |
| 2026-06-09 | Fable 5 / Mythos 5 | 10 | 1 | 50 | [Claude 价目表](https://platform.claude.com/docs/en/about-claude/pricing) |
| 2026-06-30 | Sonnet 5，8 月 10 日将首发价转为常规定价 | 2 | 0.20 | 10 | [Sonnet 5 发布公告](https://www.anthropic.com/news/claude-sonnet-5) |
| 2026-07-24 | Opus 5 | 5 | 0.50 | 25 | [平台历史记录](https://platform.claude.com/docs/en/release-notes/overview) |
| 2026-09-01 | Fable 5.1 / Mythos 5.1 | 10 | 0.25 | 50 | [发布公告](https://www.anthropic.com/claude-fable-and-mythos-5-1) |
| 2026-09-22 | Opus 5.5 | 4 | 0.20 | 20 | [Opus 5.5](https://www.anthropic.com/claude-opus-5-5) |
| 2026-09-28 至 10-06 | Sonnet 5.5 | 2 | 0.20 | 10 | [Sonnet 5.5](https://www.anthropic.com/claude-sonnet-5-5) |
| 2026-10-07 | Sonnet 5.5 | 2 | 0.10 | 10 | [缓存降价公告](https://www.anthropic.com/claude-haiku-5-5) |
| 2026-10-07 | Haiku 5.5，输入 ≤100k / >100k | 0.10 / 0.50 | 0.01 / 0.05 | 0.50 / 2.50 | [Haiku 5.5](https://www.anthropic.com/claude-haiku-5-5) |
| 2026-11-09 | Fable 5.2 / Mythos 5.2 | 8 | 0.20 | 40 | [发布公告](https://www.anthropic.com/claude-fable-and-mythos-5-2) |

Opus 4.6 Fast 在 2026-02-07 至 02-16 有五折首发优惠。短上下文输入 / 输出为 $15 / $75，长上下文为 $30 / $112.50。02-17 后两档分别为 $30 / $150 和 $60 / $225。06-29 起该模型的 Fast 请求按标准价执行。历史 CLI 包中保留了活动文案和价目表。 [Anthropic 发布的 CLI 2.1.39](https://registry.npmjs.org/@anthropic-ai/claude-code/-/claude-code-2.1.39.tgz) · [平台变更记录](https://platform.claude.com/docs/en/release-notes/overview)

Claude 的普通输入、5 分钟缓存写入、1 小时缓存写入、缓存读取和输出是五个独立计费项。写入单价分别是普通输入价的 1.25 倍和 2 倍。判断上下文长度时应加总普通输入、缓存写入和读取。例如，Sonnet 5.5 的一次请求有 10,000 普通输入、5,000 短写入、2,000 长写入、100,000 读取和 1,000 输出，在 10 月 6 日估值为 $0.0705，10 月 7 日为 $0.0605。 [Claude 定价与缓存规则](https://platform.claude.com/docs/en/about-claude/pricing)

## 其他 provider

| Provider | 查到的费率与历史变化 | OpenQuota 的费用口径 |
|---|---|---|
| Cursor | 2026-03-19 Composer 2 普通输入 / 输出为 $0.50 / $2.50，Fast 为 $1.50 / $7.50。05-18 Composer 2.5 普通价相同，Fast 为 $3 / $15。Auto 新费率取决于个人账户在 2025-09-15 起的续费日。[Composer 2](https://cursor.com/blog/composer-2) · [2.5](https://cursor.com/blog/composer-2-5) · [续费规则](https://forum.cursor.com/t/update-to-teams-and-auto-pricing/128952) | 优先使用导出行的数值 Cost，缺失时按 token 估算。 |
| Grok | 2025-08-28 Grok Code Fast 1 的输入 / 读取 / 输出为 $0.20 / $0.02 / $1.50。2026-05-15 12:00 太平洋时间起该名称重定向到 Grok Build 0.1，其基础价为 $1 / $0.20 / $2。[发布价](https://x.ai/news/grok-code-fast-1) · [逐模型迁移说明](https://docs.x.ai/developers/migration/may-15-retirement) · [当前价目表](https://docs.x.ai/developers/pricing) | 按请求时间解析旧名称。现行 ≥200k 加价的最初生效日未能核实，机器表从核对日记录该档。 |
| Kimi | K2 Turbo 在 2025-08-01 有首发折扣，11-06 将 Turbo 输入 / 读取 / 输出从 $2.40 / $0.60 / $10 调至 $1.15 / $0.15 / $8。[历史费率对照](https://platform.kimi.ai/blog/posts/Kimi_API_Newsletter) 2026 年 K3 的输入 / 读取 / 输出为 $3 / $0.30 / $15，K3.1 Pro 为 $2 / $0.20 / $10。[K2 历史公告](https://platform.kimi.ai/blog/posts/changelog) · [K3 系列定价](https://platform.kimi.ai/) | Kimi Code 配额来自账号。可识别的本地请求按 Kimi 渠道估算，K2 历次促销的完整分项仍有缺口。 |
| MiniMax | 2026-03-18 发布 M2.7。标准价输入 / 读取 / 写入 / 输出为 $0.30 / $0.06 / $0.375 / $1.20。Highspeed 为 $0.60 / $0.06 / $0.375 / $2.40。[发布公告](https://www.minimax.io/news/minimax-m27-en) · [定价](https://platform.minimax.io/subscribe/token-plan?tab=api-enterprise) | Token Plan 依账户额度显示，本地估算使用 API 费率。 |
| Z.ai | 2026-08-26 GLM-5.3-Flash 首发的输入 / 读取 / 输出为 $0.075 / $0.015 / $0.25，9 月优惠结束后为 $0.15 / $0.03 / $0.50。[模型公告](https://z.ai/blog/glm-5.3-flash) · [同期渠道价目表](https://docs.avalai.ir/en/news/2026-08-26-glm-5-3-flash-added) · [当前官方价](https://docs.z.ai/guides/overview/pricing) | Coding Plan 积分按原单位展示。历史优惠的起止依据包括渠道公告；Z.ai 当前价目表仅保留常规价。 |
| Copilot | 2025 年有超额 premium request 单价 $0.04。自 2026-06-01 起使用 GitHub AI Credits，每 credit 为 $0.01，扣多少 credit 随模型和用量变化。[旧制](https://github.blog/news-insights/product-news/github-copilot-agent-mode-activated/) · [切换公告](https://github.blog/news-insights/company-news/github-copilot-is-moving-to-usage-based-billing/) · [现行费率](https://docs.github.com/en/copilot/reference/copilot-billing/models-and-pricing) | 使用账户额度和服务端账单。 |
| Antigravity | 2026-03-12 Google 公布可在基础额度耗尽后使用 AI credits，启用后续按模型 API 费率扣减。[Google 公告](https://support.google.com/googleone/thread/416663138) Gemini 3.6 Flash 首发价为 $0.75 / $0.075 / $3.75（2027-01-01 起 $1.50 / $0.15 / $7.50），Gemini 4 Carbon 首发价为 $2 / $0.10 / $10（2026-12-01 起 $4 / $0.20 / $20），Gemini 4 Neon 为 $3 / $0.15 / $15。[Gemini API 定价](https://ai.google.dev/gemini-api/docs/pricing) | 配额依订阅方案；本地用量从 `~/.gemini/antigravity*` 对话记录汇总 token，并按对应模型和时间估算 API 等价费用。 |
| OpenCode | Zen 按路由模型计价，Go 当前为 $10/月，Go Plus 为 $40/月，Go Teams 为 $20/成员/月。模型可有独立的渠道价。[模型费率](https://opencode.ai/docs/console/models/) · [Go 计划](https://opencode.ai/docs/console/go/) | 优先使用消息或步骤金额。OpenAI、Anthropic 直连记录按对应 provider 和时间估算。 |
| OpenRouter | 推理费用依上游模型和路由。当前 Standard 购入 credits 收取 5.5%，Business 为 8%。[定价](https://openrouter.ai/pricing) | 使用账户返回的日、周、月消费。充值手续费与单次推理金额分别核算。 |
| Devin | 自助套餐按包含额度和按需 credits 计量。企业使用 ACU，单价由订单决定。[Devin Billing](https://docs.devin.ai/admin/billing) | 展示账号返回的额度和余额。统一的 ACU 美元换算率不可用。 |
| Sub2API | Codex / Claude 按实际使用的上游模型、请求时间和服务档位匹配对应费率。[管理端用量明细](https://github.com/Wei-Shaw/sub2api/blob/main/backend/internal/handler/admin/usage_handler.go) | 每个连接可选计费方式。默认「上游价格」按请求的 token 在本地估算；「Sub2API 价格」读取实例的账号统计，优先取 actual_cost（含零），缺失时取 cost。 |

### 通过其他客户端使用 DeepSeek

2026-09-10 04:00 UTC 起，`deepseek-flash` 的谷价为输入 $0.15、缓存读取 $0.003、输出 $0.60，峰价分别为 $0.30、$0.006、$1.20。高峰是周一至周五的 01:00–04:00 和 06:00–10:00 UTC，结束时刻起恢复谷价。周末和中国公共假期全天按谷价。通过 Claude 或 Codex 记录的该模型按 DeepSeek 公开价计入其他模型用量。该估算涵盖 token，独立工具调用费用需要相应的账单明细。节假日表已核对至 2027 年底。 [费率](https://api-docs.deepseek.com/quick_start/pricing/) · [生效公告](https://api-docs.deepseek.com/news/news260910/) · [2026 假期](https://www.beijing.gov.cn/zhengce/zhengcefagui/202511/t20251104_4258873.html) · [2027 假期](https://www.gov.cn/zhengce/content/202611/content_7085101.htm)

## 时间与精度

费率以请求的 UTC 时间选择，每一时期含起点、不含终点。官方只公布日期时，采用该日 00:00 UTC 作为估算边界，首日的小时级金额可能与账单有差异。美元先逐请求计算再按本地日历汇总。近 30 天包括今天和之前的 29 个自然日。已收录型号在没有适用时期或没有对应速度价格时返回未知。机器表尚未涵盖的型号沿用现有目录估价，其当前报价不能证明历史成交价。

以上为全球标准文本 token 的公开费率。Grok 长上下文费率的早期生效日以及部分旧模型的速度档位仍未核实。日志缺少缓存保留期、实际执行档位、定价渠道或专属折扣时，金额仍为估算。区域处理、独立工具费、账户抵扣以及税费可能造成账单差额。OpenAI 和 Anthropic 均提供组织级用量与费用报表，可用于对账。 [OpenAI 费用接口](https://developers.openai.com/api/reference/resources/admin/subresources/organization/subresources/usage/methods/costs) · [Claude 用量与费用接口](https://platform.claude.com/docs/en/build-with-claude/usage-cost-api)
