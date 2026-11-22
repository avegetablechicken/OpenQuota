use std::collections::HashSet;

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::Deserialize;

use super::{
    classify_status, transport_error, Envelope, Sub2ApiClient, Sub2ApiError, Sub2ApiUpstream,
};
use crate::{
    pricing::{ModelPricing, ServiceTier, TokenBreakdown},
    providers::daily_usage::DailyUsageAccumulator,
};

pub(super) const SOURCE: &str = "From Sub2API usage logs at upstream model prices";
const MAX_USAGE_PAGES: usize = 100;

#[derive(Deserialize)]
struct UsagePage {
    items: Vec<UsageLog>,
    total: usize,
}

#[derive(Deserialize)]
struct UsageLog {
    id: i64,
    account_id: i64,
    created_at: DateTime<Utc>,
    model: String,
    upstream_model: Option<String>,
    upstream_response_model: Option<String>,
    service_tier: Option<String>,
    input_tokens: u64,
    output_tokens: u64,
    #[serde(default)]
    cache_read_tokens: u64,
    #[serde(default)]
    cache_creation_tokens: u64,
    #[serde(default)]
    cache_creation_5m_tokens: u64,
    #[serde(default)]
    cache_creation_1h_tokens: u64,
    #[serde(default)]
    cache_creation_30m_tokens: u64,
    #[serde(default)]
    cache_creation_12h_tokens: u64,
    #[serde(default)]
    image_count: u64,
    #[serde(default)]
    image_output_tokens: u64,
    billing_mode: Option<String>,
}

pub(super) struct PricedUsage {
    date: NaiveDate,
    model: String,
    tokens: u64,
    cost: Option<f64>,
}

impl PricedUsage {
    pub(super) fn add_to(&self, history: &mut DailyUsageAccumulator) {
        match self.cost {
            Some(cost) => history.add(self.date, self.tokens, cost, &self.model),
            None => history.add_unpriced(self.date, self.tokens, &self.model),
        }
    }
}

impl UsageLog {
    fn priced(self, upstream: Sub2ApiUpstream, pricing: &ModelPricing) -> Option<PricedUsage> {
        let model = [
            self.upstream_response_model.as_deref(),
            self.upstream_model.as_deref(),
            Some(&self.model),
        ]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|name| !name.is_empty())
        .unwrap_or("Unattributed");
        // Sub2API's stored input excludes both cache reads and writes for either upstream.
        // An aggregate write count can coexist with a partial TTL breakdown on older logs.
        let (short, one_hour, twelve_hours) = match upstream {
            Sub2ApiUpstream::Codex => (
                self.cache_creation_30m_tokens,
                0,
                self.cache_creation_12h_tokens,
            ),
            Sub2ApiUpstream::Claude => (
                self.cache_creation_5m_tokens,
                self.cache_creation_1h_tokens,
                0,
            ),
        };
        let long = one_hour + twelve_hours;
        let detailed = short.saturating_add(long);
        let writes = if self.cache_creation_tokens == 0 {
            detailed
        } else {
            self.cache_creation_tokens
        };
        let tokens = TokenBreakdown {
            input: self.input_tokens,
            output: self.output_tokens,
            cache_read: self.cache_read_tokens,
            cache_write_5m: writes.saturating_sub(long),
            cache_write_1h: one_hour.min(writes),
            cache_write_12h: twelve_hours.min(writes),
            ..Default::default()
        };
        let image = self.image_count > 0
            || self.image_output_tokens > 0
            || self.billing_mode.as_deref() == Some("image");
        if !image && tokens.total_tokens() == 0 {
            return None;
        }
        let tier = self
            .service_tier
            .as_deref()
            .map(str::trim)
            .filter(|tier| !tier.is_empty())
            .map(ServiceTier::parse)
            .unwrap_or(ServiceTier::Standard);
        let cost = if image || detailed > writes || model == "Unattributed" {
            None
        } else if upstream == Sub2ApiUpstream::Codex
            && model.eq_ignore_ascii_case("codex-auto-review")
        {
            // Account discovery is restricted to upstream OAuth accounts.
            pricing
                .historical_rates(
                    "codex-chatgpt",
                    model,
                    self.created_at,
                    ServiceTier::Standard,
                    tokens.prompt_tokens(),
                )
                .flatten()
                .map(|rates| rates.cost_dollars(tokens, true))
        } else {
            pricing.upstream_cost_at(upstream.provider_id(), model, self.created_at, tokens, tier)
        };
        Some(PricedUsage {
            date: self.created_at.with_timezone(&Local).date_naive(),
            model: model.to_owned(),
            tokens: tokens.total_tokens(),
            cost,
        })
    }
}

impl Sub2ApiClient {
    pub(super) fn priced_usage(
        &self,
        token: &str,
        account_id: i64,
        upstream: Sub2ApiUpstream,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<Vec<PricedUsage>, Sub2ApiError> {
        self.priced_usage_with_max_pages(token, account_id, upstream, now, pricing, MAX_USAGE_PAGES)
    }

    fn priced_usage_with_max_pages(
        &self,
        token: &str,
        account_id: i64,
        upstream: Sub2ApiUpstream,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
        max_pages: usize,
    ) -> Result<Vec<PricedUsage>, Sub2ApiError> {
        let today = now.with_timezone(&Local).date_naive();
        let since = today
            .checked_sub_days(Days::new(29))
            .unwrap_or(NaiveDate::MIN);
        // Query a UTC envelope and apply the local calendar boundary to each timestamp.
        let mut base = self.endpoint(&["api", "v1", "admin", "usage"])?;
        base.query_pairs_mut()
            .append_pair("account_id", &account_id.to_string())
            .append_pair("start_date", &since.pred_opt().unwrap_or(since).to_string())
            .append_pair("end_date", &now.date_naive().to_string())
            .append_pair("timezone", "UTC")
            .append_pair("page_size", "1000")
            .append_pair("sort_by", "id")
            .append_pair("sort_order", "asc");
        let mut rows = Vec::new();
        let mut seen = HashSet::new();
        let mut page = 1_usize;
        loop {
            let mut url = base.clone();
            url.query_pairs_mut().append_pair("page", &page.to_string());
            let response = self
                .client
                .get(url)
                .bearer_auth(token)
                .header("Accept", "application/json")
                .send()
                .map_err(|err| transport_error("upstream usage records", &err))?;
            let status = response.status();
            if !status.is_success() {
                return Err(classify_status(status));
            }
            let envelope = response
                .json::<Envelope<UsagePage>>()
                .map_err(|_| Sub2ApiError::InvalidResponse)?;
            if envelope.code != 0 {
                return Err(Sub2ApiError::InvalidResponse);
            }
            let result = envelope.data.ok_or(Sub2ApiError::InvalidResponse)?;
            let page_empty = result.items.is_empty();
            let previous = seen.len();
            for row in result.items {
                if row.account_id != account_id || row.id <= 0 {
                    return Err(Sub2ApiError::InvalidResponse);
                }
                if !seen.insert(row.id)
                    || row.created_at > now
                    || row.created_at.with_timezone(&Local).date_naive() < since
                {
                    continue;
                }
                rows.extend(row.priced(upstream, pricing));
            }
            if seen.len() >= result.total {
                return Ok(rows);
            }
            if page_empty {
                return if page == 1 {
                    Err(Sub2ApiError::InvalidResponse)
                } else {
                    Ok(rows)
                };
            }
            if seen.len() == previous {
                return Err(Sub2ApiError::InvalidResponse);
            }
            if page >= max_pages {
                return Err(Sub2ApiError::InvalidResponse);
            }
            page += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pricing::test_bundled_pricing;
    use serde_json::json;

    #[test]
    fn codex_prices_the_response_model_tier_and_entire_prompt() {
        let row: UsageLog = serde_json::from_value(json!({
            "id":1,"account_id":7,"created_at":"2026-11-18T12:00:00Z",
            "model":"client-alias","upstream_model":"gpt-6.2-astra","upstream_response_model":"gpt-6.2-sol",
            "service_tier":"priority","input_tokens":100000,"cache_read_tokens":165000,
            "cache_creation_tokens":12000,"cache_creation_30m_tokens":10000,"cache_creation_12h_tokens":2000,
            "output_tokens":1000,"cost":90,"actual_cost":80,"account_stats_cost":70
        })).unwrap();
        let row = row
            .priced(Sub2ApiUpstream::Codex, &test_bundled_pricing())
            .unwrap();
        assert_eq!(row.model, "gpt-6.2-sol");
        assert_eq!(row.tokens, 278000);
        assert!((row.cost.unwrap() - 1.036).abs() < 1e-12);
    }

    #[test]
    fn claude_prices_each_timestamp_and_preserves_unpriced_usage() {
        let pricing = test_bundled_pricing();
        let record = |date, model| {
            serde_json::from_value::<UsageLog>(json!({
            "id":1,"account_id":7,"created_at":date,"model":model,
            "input_tokens":10000,"cache_read_tokens":100000,"cache_creation_tokens":7000,
            "cache_creation_1h_tokens":2000,"output_tokens":1000,"actual_cost":0,"total_cost":100
        })).unwrap().priced(Sub2ApiUpstream::Claude, &pricing).unwrap()
        };
        let before = record("2026-10-06T23:59:59Z", "claude-sonnet-5-5");
        let after = record("2026-10-07T00:00:00Z", "claude-sonnet-5-5");
        assert!((before.cost.unwrap() - 0.0705).abs() < 1e-12);
        assert!((after.cost.unwrap() - 0.0605).abs() < 1e-12);
        let unknown = record("2026-10-07T00:00:00Z", "unpriced-custom-model");
        assert_eq!(unknown.cost, None);
        assert_eq!(unknown.tokens, 118000);
    }

    #[test]
    fn pages_deduplicate_ids_and_filter_by_account_and_time() {
        let now: DateTime<Utc> = "2026-11-20T12:00:00Z".parse().unwrap();
        let entry = |id, date| json!({"id":id,"account_id":7,"created_at":date,"model":"gpt-6.2-sol","input_tokens":100,"output_tokens":10});
        let first = entry(1, "2026-11-18T12:00:00Z");
        let second = entry(2, "2026-11-19T12:00:00Z");
        let page = |rows: Vec<serde_json::Value>, total| {
            json!({"code":0,"data":{"items":rows,"total":total}}).to_string()
        };
        let (base, requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![first.clone(), second.clone()], 4),
            page(
                vec![
                    second.clone(),
                    entry(3, "2026-10-01T12:00:00Z"),
                    entry(4, "2026-11-20T13:00:00Z"),
                ],
                4,
            ),
        ]);
        let pricing = test_bundled_pricing();
        let fetch = |base: &str| {
            Sub2ApiClient::new(base).unwrap().priced_usage(
                "token",
                7,
                Sub2ApiUpstream::Codex,
                now,
                &pricing,
            )
        };
        assert_eq!(fetch(&base).unwrap().len(), 2);
        worker.join().unwrap();
        assert!(requests.recv().unwrap().contains("page=1 "));
        assert!(requests.recv().unwrap().contains("page=2 "));
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![first.clone()], 2),
            page(vec![first.clone()], 2),
        ]);
        assert!(matches!(fetch(&base), Err(Sub2ApiError::InvalidResponse)));
        worker.join().unwrap();
        let mut foreign = first;
        foreign["account_id"] = 8.into();
        let (base, _requests, worker) =
            super::super::tests::serve_sequence(vec![page(vec![foreign], 1)]);
        assert!(matches!(fetch(&base), Err(Sub2ApiError::InvalidResponse)));
        worker.join().unwrap();
    }

    #[test]
    fn pagination_handles_log_pruning_shrinking_total_and_empty_trailing_page() {
        let now: DateTime<Utc> = "2026-11-20T12:00:00Z".parse().unwrap();
        let entry = |id, date| {
            json!({
                "id": id,
                "account_id": 7,
                "created_at": date,
                "model": "gpt-6.2-sol",
                "input_tokens": 100,
                "output_tokens": 10
            })
        };
        let page = |rows: Vec<serde_json::Value>, total| {
            json!({"code":0,"data":{"items":rows,"total":total}}).to_string()
        };
        let pricing = test_bundled_pricing();
        let fetch = |base: &str| {
            Sub2ApiClient::new(base).unwrap().priced_usage(
                "token",
                7,
                Sub2ApiUpstream::Codex,
                now,
                &pricing,
            )
        };

        // Subcase A: total shrinks between page 1 and page 2 due to log pruning.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![entry(1, "2026-11-18T12:00:00Z")], 3),
            page(vec![entry(2, "2026-11-19T12:00:00Z")], 2),
        ]);
        assert_eq!(fetch(&base).unwrap().len(), 2);
        worker.join().unwrap();

        // Subcase B: trailing page is empty after earlier records were pruned.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![entry(1, "2026-11-18T12:00:00Z")], 2),
            page(vec![], 2),
        ]);
        assert_eq!(fetch(&base).unwrap().len(), 1);
        worker.join().unwrap();

        // Subcase C: empty page 1 contradicting total > 0 is invalid.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![page(vec![], 2)]);
        assert!(matches!(fetch(&base), Err(Sub2ApiError::InvalidResponse)));
        worker.join().unwrap();

        // Subcase D: empty page 1 with total == 0 succeeds with empty rows.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![page(vec![], 0)]);
        assert!(fetch(&base).unwrap().is_empty());
        worker.join().unwrap();
    }

    #[test]
    fn pagination_rejects_responses_exceeding_max_usage_pages() {
        let now: DateTime<Utc> = "2026-11-20T12:00:00Z".parse().unwrap();
        let entry = |id| {
            json!({
                "id": id,
                "account_id": 7,
                "created_at": "2026-11-19T12:00:00Z",
                "model": "gpt-6.2-sol",
                "input_tokens": 100,
                "output_tokens": 10
            })
        };
        let page = |rows: Vec<serde_json::Value>, total| {
            json!({"code":0,"data":{"items":rows,"total":total}}).to_string()
        };
        let pricing = test_bundled_pricing();

        // Exactly max_pages (2) succeeds.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![entry(1)], 2),
            page(vec![entry(2)], 2),
        ]);
        let rows = Sub2ApiClient::new(&base)
            .unwrap()
            .priced_usage_with_max_pages("token", 7, Sub2ApiUpstream::Codex, now, &pricing, 2)
            .unwrap();
        assert_eq!(rows.len(), 2);
        worker.join().unwrap();

        // Exceeding max_pages (2) fails without requesting page 3.
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            page(vec![entry(1)], 3),
            page(vec![entry(2)], 3),
        ]);
        let result = Sub2ApiClient::new(&base)
            .unwrap()
            .priced_usage_with_max_pages("token", 7, Sub2ApiUpstream::Codex, now, &pricing, 2);
        assert!(matches!(result, Err(Sub2ApiError::InvalidResponse)));
        worker.join().unwrap();
    }
}
