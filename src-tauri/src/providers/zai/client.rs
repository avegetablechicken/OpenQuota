use std::time::Duration;

use reqwest::{blocking::Client, StatusCode, Url};
use serde_json::Value;

use super::ZaiError;

const SUBSCRIPTION_URL: &str = "https://api.z.ai/api/biz/subscription/list";
const QUOTA_URL: &str = "https://api.z.ai/api/monitor/usage/quota/limit";
const ZAI_LEGACY_USAGE_URL: &str = "https://api.z.ai/api/monitor/usage/model-usage";
const ZAI_CREDIT_USAGE_URL: &str = "https://api.z.ai/api/monitor/credit-usage/usage-detail";

#[derive(Debug, Clone, Copy)]
pub enum AccountUsageKind {
    Legacy,
    Credits,
}

#[derive(Debug)]
pub struct ZaiResponse {
    pub status: StatusCode,
    pub body: Value,
}

pub struct ZaiClient {
    client: Client,
    subscription_url: String,
    quota_url: String,
    legacy_usage_url: String,
    credit_usage_url: String,
}

impl ZaiClient {
    pub fn new() -> Result<Self, ZaiError> {
        Self::with_endpoints(
            SUBSCRIPTION_URL,
            QUOTA_URL,
            ZAI_LEGACY_USAGE_URL,
            ZAI_CREDIT_USAGE_URL,
            Duration::from_secs(15),
        )
    }

    fn with_endpoints(
        subscription_url: &str,
        quota_url: &str,
        legacy_usage_url: &str,
        credit_usage_url: &str,
        timeout: Duration,
    ) -> Result<Self, ZaiError> {
        let client = crate::http_client::blocking_client_builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(timeout)
            .user_agent(concat!("OpenQuota/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|_| ZaiError::ConnectionFailed)?;
        Ok(Self {
            client,
            subscription_url: subscription_url.to_owned(),
            quota_url: quota_url.to_owned(),
            legacy_usage_url: legacy_usage_url.to_owned(),
            credit_usage_url: credit_usage_url.to_owned(),
        })
    }

    pub fn fetch_quota(&self, api_key: &str) -> Result<ZaiResponse, ZaiError> {
        self.fetch(&self.quota_url, api_key, "quota")
    }

    pub fn fetch_subscription(&self, api_key: &str) -> Result<ZaiResponse, ZaiError> {
        self.fetch(&self.subscription_url, api_key, "subscription")
    }

    pub fn fetch_account_usage(
        &self,
        kind: AccountUsageKind,
        api_key: &str,
        start_time: &str,
        end_time: &str,
    ) -> Result<ZaiResponse, ZaiError> {
        let (url, endpoint) = match kind {
            AccountUsageKind::Legacy => (&self.legacy_usage_url, "legacy account usage"),
            AccountUsageKind::Credits => (&self.credit_usage_url, "credit account usage"),
        };
        let mut url = Url::parse(url).map_err(|_| ZaiError::InvalidResponse)?;
        {
            let mut query = url.query_pairs_mut();
            query
                .append_pair("startTime", start_time)
                .append_pair("endTime", end_time)
                .append_pair("type", "1");
            if matches!(kind, AccountUsageKind::Credits) {
                query.append_pair("usageType", "MODEL");
            }
        }
        let request = self
            .client
            .get(url)
            .bearer_auth(api_key)
            .header("Accept", "application/json");
        self.send(request, endpoint)
    }

    fn fetch(&self, url: &str, api_key: &str, endpoint: &str) -> Result<ZaiResponse, ZaiError> {
        let request = self
            .client
            .get(url)
            .bearer_auth(api_key)
            .header("Accept", "application/json");
        self.send(request, endpoint)
    }

    fn send(
        &self,
        request: reqwest::blocking::RequestBuilder,
        endpoint: &str,
    ) -> Result<ZaiResponse, ZaiError> {
        let started = std::time::Instant::now();
        let response = request.send().map_err(|_| {
            crate::app_warn!("http", "zai {endpoint} request failed (transport)");
            ZaiError::ConnectionFailed
        })?;
        let status = response.status();
        crate::app_debug!(
            "http",
            "zai {endpoint} HTTP {} ({}ms)",
            status.as_u16(),
            started.elapsed().as_millis()
        );
        let text = response.text().map_err(|_| ZaiError::InvalidResponse)?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(ZaiResponse { status, body })
    }
}

#[cfg(test)]
impl ZaiClient {
    pub fn for_test(
        subscription_url: &str,
        quota_url: &str,
        legacy_usage_url: &str,
        credit_usage_url: &str,
        timeout: Duration,
    ) -> Self {
        Self::with_endpoints(
            subscription_url,
            quota_url,
            legacy_usage_url,
            credit_usage_url,
            timeout,
        )
        .unwrap()
    }
}
