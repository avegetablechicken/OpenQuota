use rusqlite::params;

use chrono::{DateTime, Days, Local, NaiveDate, Utc};
use serde::{Deserialize, Serialize};

use super::{
    cache::{utc_start, UsageCache},
    classify_status, transport_error, Envelope, Sub2ApiClient, Sub2ApiError, Sub2ApiUpstream,
};
use crate::{
    pricing::{ModelPricing, ServiceTier, TokenBreakdown},
    providers::daily_usage::DailyUsageAccumulator,
};

pub(super) const SOURCE: &str = "From Sub2API usage logs at upstream model prices";
const PAGE_SIZE: usize = 128;

#[derive(Deserialize)]
struct UsagePage {
    items: Vec<UsageLog>,
    total: usize,
    page_size: Option<usize>,
}

#[derive(Deserialize, Serialize)]
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
            Sub2ApiUpstream::Claude | Sub2ApiUpstream::Antigravity | Sub2ApiUpstream::Grok => (
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
            let provider = match upstream {
                Sub2ApiUpstream::Antigravity => {
                    crate::providers::antigravity::billing_provider(model)
                }
                _ => upstream.provider_id(),
            };
            pricing.upstream_cost_at(provider, model, self.created_at, tokens, tier)
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
    fn usage_page(
        &self,
        base: &reqwest::Url,
        token: &str,
        page: usize,
        size: usize,
    ) -> Result<UsagePage, Sub2ApiError> {
        let mut url = base.clone();
        url.query_pairs_mut()
            .append_pair("page_size", &size.to_string())
            .append_pair("sort_order", "asc")
            .append_pair("page", &page.to_string());
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .header("Accept", "application/json")
            .send()
            .map_err(|err| transport_error("upstream usage records", &err))?;
        if !response.status().is_success() {
            return Err(classify_status(response.status()));
        }
        let envelope = response
            .json::<Envelope<UsagePage>>()
            .map_err(|_| Sub2ApiError::InvalidResponse)?;
        if envelope.code != 0 {
            return Err(Sub2ApiError::InvalidResponse);
        }
        envelope.data.ok_or(Sub2ApiError::InvalidResponse)
    }

    /// Persist complete pages as they arrive. Publish totals only once all records
    /// for this account's 30-day window are available.
    pub(super) fn priced_usage(
        &self,
        token: &str,
        account: &super::Sub2ApiAccount,
        upstream: Sub2ApiUpstream,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
        cache: &mut UsageCache,
    ) -> Result<Vec<PricedUsage>, Sub2ApiError> {
        let today = now.with_timezone(&Local).date_naive();
        let since = today
            .checked_sub_days(Days::new(29))
            .unwrap_or(NaiveDate::MIN);
        let account_id = account.id;
        let start = utc_start(since);
        let end = now.date_naive();
        cache
            .db
            .execute("DELETE FROM records WHERE day < ?1", [start.to_string()])?;
        // Older servers may omit last_used_at. A one-row boundary check also catches
        // retention/administrative changes periodically on servers that expose it.
        let fresh: bool = cache.db.query_row(
            "SELECT EXISTS(SELECT 1 FROM synced
             WHERE account=?1 AND activity=?2 AND start=?3 AND end=?4 AND checked BETWEEN ?5 AND ?6)",
            params![account_id, account.last_used_at, start.to_string(), end.to_string(),
                    now.timestamp() - 3600, now.timestamp()],
            |row| row.get(0),
        )?;
        if !fresh {
            cache
                .db
                .execute("DELETE FROM synced WHERE account=?1", [account_id])?;
            self.sync_range(token, account_id, start, end, cache)?;
            if let Some(activity) = &account.last_used_at {
                cache.db.execute(
                    "INSERT INTO synced(account,activity,start,end,checked) VALUES(?1,?2,?3,?4,?5)",
                    params![
                        account_id,
                        activity,
                        start.to_string(),
                        end.to_string(),
                        now.timestamp()
                    ],
                )?;
            }
        }
        let mut statement = cache.db.prepare(
            "SELECT payload FROM records WHERE account=?1 AND day BETWEEN ?2 AND ?3 ORDER BY id",
        )?;
        let mut records =
            statement.query(params![account_id, start.to_string(), end.to_string()])?;
        let mut priced = Vec::new();
        while let Some(record) = records.next()? {
            let payload: String = record.get(0)?;
            let row: UsageLog =
                serde_json::from_str(&payload).map_err(|_| Sub2ApiError::HistoryCache)?;
            if row.created_at <= now && row.created_at.with_timezone(&Local).date_naive() >= since {
                priced.extend(row.priced(upstream, pricing));
            }
        }
        Ok(priced)
    }

    fn sync_range(
        &self,
        token: &str,
        account_id: i64,
        from: NaiveDate,
        to: NaiveDate,
        cache: &mut UsageCache,
    ) -> Result<(), Sub2ApiError> {
        let start = from.to_string();
        let end = to.to_string();
        let mut url = self.endpoint(&["api", "v1", "admin", "usage"])?;
        url.query_pairs_mut()
            .append_pair("account_id", &account_id.to_string())
            .append_pair("start_date", &start)
            .append_pair("end_date", &end)
            .append_pair("timezone", "UTC")
            .append_pair("exact_total", "true")
            .append_pair("sort_by", "id");
        let (count, mut last_id): (i64, i64) = cache.db.query_row(
            "SELECT count(*), coalesce(max(id),0) FROM records
             WHERE account=?1 AND day BETWEEN ?2 AND ?3",
            params![account_id, start, end],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;

        let mut offset = usize::try_from(count).map_err(|_| Sub2ApiError::HistoryCache)?;

        // The API exposes numbered pages, not an ID cursor. Verify the record at our
        // saved offset before using it. Appends preserve this prefix, deletions do not.
        let mut total;
        let mut page_limit = PAGE_SIZE;
        if offset > 0 {
            let boundary = self.usage_page(&url, token, offset, 1)?;
            total = boundary.total;
            if boundary.items.len() != usize::from(total >= offset)
                || boundary
                    .items
                    .iter()
                    .any(|r| r.account_id != account_id || r.id <= 0)
            {
                return Err(Sub2ApiError::InvalidResponse);
            }
            if boundary.items.first().map(|r| r.id) != Some(last_id) {
                if from < to {
                    // A retention change moved an offset. Reconcile individual UTC days
                    // so a deletion on one day cannot invalidate the rest of the month.
                    let mut day = from;
                    loop {
                        self.sync_range(token, account_id, day, day, cache)?;
                        if day == to {
                            return Ok(());
                        }
                        day = day.succ_opt().ok_or(Sub2ApiError::InvalidResponse)?;
                    }
                }
                cache.db.execute(
                    "DELETE FROM records WHERE account=?1 AND day BETWEEN ?2 AND ?3",
                    params![account_id, start, end],
                )?;
                offset = 0;
                last_id = 0;
            }
        } else {
            total = 0;
        }
        if offset == 0 {
            cache.take_page()?;
            let first = self.usage_page(&url, token, 1, PAGE_SIZE)?;
            total = first.total;
            offset = first.items.len();
            page_limit = first.page_size.unwrap_or(PAGE_SIZE).min(PAGE_SIZE);
            if page_limit == 0 || offset != total.min(page_limit) {
                return Err(Sub2ApiError::InvalidResponse);
            }
            if first.items.is_empty() {
                cache.refund_empty_page();
                return Ok(());
            }
            last_id = store_page(cache, account_id, &start, &end, last_id, first.items)?;
        }
        while offset < total {
            cache.take_page()?;
            // Choose an aligned size so the first returned row is exactly the first
            // missing offset, including on servers that only support page/page_size.
            let mut size = PAGE_SIZE;
            while size > page_limit || size > total - offset || !offset.is_multiple_of(size) {
                size /= 2;
            }
            let page = self.usage_page(&url, token, offset / size + 1, size)?;
            if let Some(limit) = page.page_size.filter(|limit| *limit > 0 && *limit < size) {
                // A capped page uses the server's offset. Recompute it before accepting rows.
                page_limit = limit;
                continue;
            }
            if page.total < total || page.items.len() != size {
                return Err(Sub2ApiError::InvalidResponse);
            }
            last_id = store_page(cache, account_id, &start, &end, last_id, page.items)?;
            offset += size;
        }
        Ok(())
    }
}

fn store_page(
    cache: &mut UsageCache,
    account_id: i64,
    start: &str,
    end: &str,
    mut last_id: i64,
    rows: Vec<UsageLog>,
) -> Result<i64, Sub2ApiError> {
    let tx = cache.db.transaction()?;
    for row in rows {
        let day = row.created_at.date_naive().to_string();
        if row.account_id != account_id
            || row.id <= last_id
            || day.as_str() < start
            || day.as_str() > end
        {
            return Err(Sub2ApiError::InvalidResponse);
        }
        last_id = row.id;
        let payload = serde_json::to_string(&row).map_err(|_| Sub2ApiError::HistoryCache)?;
        tx.execute(
            "INSERT INTO records(account,id,day,payload) VALUES(?1,?2,?3,?4)",
            params![account_id, row.id, day, payload],
        )?;
    }
    tx.commit()?;
    Ok(last_id)
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
    fn antigravity_and_grok_use_their_native_rate_cards() {
        let pricing = test_bundled_pricing();
        let record = |upstream, model: &str| {
            serde_json::from_value::<UsageLog>(json!({
                "id":1,"account_id":7,"created_at":"2026-11-23T12:00:00Z",
                "model":"client-alias","upstream_model":model,
                "input_tokens":10000,"cache_read_tokens":1000,"output_tokens":100
            }))
            .unwrap()
            .priced(upstream, &pricing)
            .unwrap()
        };
        let tokens = TokenBreakdown {
            input: 10000,
            output: 100,
            cache_read: 1000,
            ..Default::default()
        };
        let at = "2026-11-23T12:00:00Z".parse().unwrap();
        for (upstream, model, provider) in [
            (
                Sub2ApiUpstream::Antigravity,
                "gemini-4-neon-high",
                "antigravity",
            ),
            (
                Sub2ApiUpstream::Antigravity,
                "claude-opus-5-5-high",
                "claude",
            ),
            (Sub2ApiUpstream::Grok, "grok-4.6", "grok"),
        ] {
            let row = record(upstream, model);
            let expected = pricing
                .upstream_cost_at(provider, model, at, tokens, ServiceTier::Standard)
                .unwrap_or_else(|| panic!("{model} has no {provider} rate"));
            assert_eq!(row.model, model);
            assert_eq!(row.tokens, 11100);
            assert!((row.cost.unwrap() - expected).abs() < 1e-12, "{model}");
        }
    }

    fn usage_page(ids: impl IntoIterator<Item = i64>, total: usize, now: DateTime<Utc>) -> String {
        let rows: Vec<_> = ids
            .into_iter()
            .map(|id| {
                json!({
                    "id":id, "account_id":7, "model":"gpt-6.2-sol", "created_at":now,
                    "input_tokens":100, "output_tokens":1, "request_id":"unused-server-field"
                })
            })
            .collect();
        json!({"code":0,"data":{"items":rows,"total":total}}).to_string()
    }

    #[test]
    fn resumes_after_restart_and_downloads_only_the_appended_rows() {
        let now = Utc::now();
        let (base, requests, worker) = super::super::tests::serve_sequence(vec![
            usage_page(1..=128, 130, now),
            usage_page([129, 129], 130, now), // Invalid second row rolls the entire page back.
            usage_page([128], 131, now),
            usage_page([129, 130], 131, now),
            usage_page([131], 131, now),
            usage_page([131], 132, now),
            usage_page([132], 132, now),
            usage_page([132], 132, now),
        ]);
        let client = Sub2ApiClient::new(&base).unwrap();
        let pricing = test_bundled_pricing();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.sqlite3");
        let fetch = |activity: Option<&str>, time| {
            let mut cache = UsageCache::open(&path, "instance-a").unwrap();
            let account = super::super::Sub2ApiAccount {
                id: 7,
                name: "test".into(),
                last_used_at: activity.map(str::to_owned),
            };
            client.priced_usage(
                "token",
                &account,
                Sub2ApiUpstream::Codex,
                time,
                &pricing,
                &mut cache,
            )
        };
        assert!(matches!(
            fetch(Some("A"), now),
            Err(Sub2ApiError::InvalidResponse)
        ));
        assert_eq!(fetch(Some("A"), now).unwrap().len(), 131);
        assert_eq!(fetch(Some("A"), now).unwrap().len(), 131); // Another restart, no request.
        assert_eq!(fetch(Some("B"), now).unwrap().len(), 132);
        assert_eq!(fetch(None, now).unwrap().len(), 132); // Legacy server: one row verifies the cache.
        worker.join().unwrap();
        let sizes_and_pages: Vec<_> = requests
            .try_iter()
            .map(|r| {
                let path = r.split_whitespace().nth(1).unwrap();
                let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                let query: std::collections::HashMap<_, _> = url.query_pairs().collect();
                (
                    query["page_size"].parse::<usize>().unwrap(),
                    query["page"].parse::<usize>().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            sizes_and_pages,
            [
                (128, 1),
                (2, 65),
                (1, 128),
                (2, 65),
                (1, 131),
                (1, 131),
                (1, 132),
                (1, 132)
            ]
        );
        let cache = UsageCache::open(&path, "instance-b").unwrap();
        let count: i64 = cache
            .db
            .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn a_pruned_day_is_repaired_without_reloading_unchanged_days() {
        let now = Utc::now();
        let day = now.date_naive();
        let prev = now - chrono::Duration::days(1);
        let (base, requests, worker) = super::super::tests::serve_sequence(vec![
            usage_page([1], 1, prev),
            usage_page([2, 3], 2, now),
            usage_page([], 2, now), // Old offset three no longer exists.
            usage_page([1], 1, prev),
            usage_page([], 1, now),
            usage_page([3], 1, now),
        ]);
        let client = Sub2ApiClient::new(&base).unwrap();
        let mut cache = UsageCache::open(std::path::Path::new(":memory:"), "test").unwrap();
        for (from, to) in [
            (prev.date_naive(), prev.date_naive()),
            (day, day),
            (prev.date_naive(), day),
        ] {
            client.sync_range("token", 7, from, to, &mut cache).unwrap();
        }
        let ids: Vec<i64> = cache
            .db
            .prepare("SELECT id FROM records ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(ids, [1, 3]);
        worker.join().unwrap();
        let seen: Vec<_> = requests.try_iter().collect();
        assert!(seen[3].contains("page_size=1&"));
        assert!(seen[5].contains(&format!("start_date={day}&end_date={day}")));
    }

    #[test]
    fn automatic_history_sync_resumes_the_full_window_without_publishing_partial_usage() {
        let now = Utc::now();
        let past = now - chrono::Duration::days(10);
        let (base, requests, worker) = super::super::tests::serve_sequence(vec![
            usage_page(1..=128, 129, past),
            usage_page([128], 129, past),
            usage_page([129], 129, now),
        ]);
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.db");
        let client = Sub2ApiClient::new(&base).unwrap();
        let pricing = test_bundled_pricing();
        let account = super::super::Sub2ApiAccount {
            id: 7,
            name: String::new(),
            last_used_at: Some("A".into()),
        };
        let fetch = |cache: &mut UsageCache| {
            client.priced_usage(
                "token",
                &account,
                Sub2ApiUpstream::Codex,
                now,
                &pricing,
                cache,
            )
        };
        let mut cache = UsageCache::open(&path, "test").unwrap();
        for _ in 0..15 {
            cache.take_page().unwrap();
        }
        assert!(matches!(
            fetch(&mut cache),
            Err(Sub2ApiError::HistorySyncPending)
        ));
        drop(cache);
        let mut cache = UsageCache::open(&path, "test").unwrap();
        assert_eq!(fetch(&mut cache).unwrap().len(), 129);
        worker.join().unwrap();
        let requests: Vec<_> = requests.try_iter().collect();
        let since = now.with_timezone(&Local).date_naive() - Days::new(29);
        assert!(requests[0].contains(&format!("start_date={}&", utc_start(since))));
        assert!(requests[2].contains("page=129 "));
    }

    #[test]
    fn rejects_foreign_rows_and_contradictory_pages_without_saving_them() {
        let now = Utc::now();
        let mut foreign: serde_json::Value =
            serde_json::from_str(&usage_page([1, 2], 2, now)).unwrap();
        foreign["data"]["items"][1]["account_id"] = 8.into();
        let (base, _requests, worker) = super::super::tests::serve_sequence(vec![
            foreign.to_string(),
            usage_page([], 1, now),
            usage_page([4], 1, now),
        ]);
        let client = Sub2ApiClient::new(&base).unwrap();
        let mut cache = UsageCache::open(std::path::Path::new(":memory:"), "test").unwrap();
        for _ in 0..2 {
            assert_eq!(
                client.sync_range("token", 7, now.date_naive(), now.date_naive(), &mut cache),
                Err(Sub2ApiError::InvalidResponse)
            );
            assert_eq!(
                cache
                    .db
                    .query_row("SELECT count(*) FROM records", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        client
            .sync_range("token", 7, now.date_naive(), now.date_naive(), &mut cache)
            .unwrap();
        worker.join().unwrap();
    }
}
