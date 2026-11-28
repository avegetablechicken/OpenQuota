mod client;
mod database;
mod mapper;
mod paths;
mod record;
mod scanner;

use std::sync::Arc;

use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::{
    models::{
        MetricDefinition, MetricSection, ProviderDefinition, ProviderErrorKind, ProviderLink,
        ProviderSnapshot, UsageHistories, UsagePeriodSelection,
    },
    pricing::{ModelPricing, PricingStore},
};

use self::{
    client::OpenCodeClient,
    mapper::map_go_usage,
    paths::OpenCodePaths,
    scanner::{OpenCodeUsageScanner, USAGE_SOURCE_NOTE},
};

use super::{ProviderError, UsageProvider};

pub(crate) fn scan_routed_usage_into(
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
    accumulator: &mut super::local_usage::card_usage::DailyUsageAccumulator,
) -> Result<bool, OpenCodeError> {
    OpenCodeUsageScanner::new(OpenCodePaths::new()).scan_into(now, pricing, card_id, accumulator)
}

pub(crate) fn definition() -> ProviderDefinition {
    ProviderDefinition {
        id: "opencode".into(),
        display_name: "OpenCode".into(),
        short_name: "OC".into(),
        fallback_enabled: false,
        local_usage_source_note: Some(USAGE_SOURCE_NOTE.into()),
        links: vec![ProviderLink::new("Dashboard", "https://opencode.ai/auth")],
        metrics: vec![
            MetricDefinition::quota(
                "opencode.session",
                "Session",
                "session",
                true,
                true,
                MetricSection::AlwaysVisible,
                false,
                "S",
            ),
            MetricDefinition::quota(
                "opencode.weekly",
                "Weekly",
                "weekly",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "W",
            ),
            MetricDefinition::quota(
                "opencode.monthly",
                "Monthly",
                "monthly",
                false,
                true,
                MetricSection::AlwaysVisible,
                false,
                "M",
            ),
            MetricDefinition::trend("opencode.trend"),
            MetricDefinition::usage(
                "opencode.today",
                "Today",
                UsagePeriodSelection::Today,
                MetricSection::OnDemand,
                "T",
            ),
            MetricDefinition::usage(
                "opencode.yesterday",
                "Yesterday",
                UsagePeriodSelection::Yesterday,
                MetricSection::OnDemand,
                "Y",
            ),
            MetricDefinition::usage(
                "opencode.last30",
                "Last 30 Days",
                UsagePeriodSelection::Last30Days,
                MetricSection::OnDemand,
                "30",
            ),
        ],
    }
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpenCodeError {
    #[error("OpenCode was not detected. Sign in to OpenCode Go or use OpenCode locally first.")]
    NotDetected,
    #[error("OpenCode login data could not be read. Sign in to OpenCode Go again.")]
    CredentialsUnreadable,
    #[error("The OpenCode data directory could not be read.")]
    DataDirectoryUnreadable,
    #[error("OpenCode local usage data is temporarily unavailable.")]
    DatabaseUnreadable,
    #[error("OpenCode Go login data is invalid or expired. Sign in to OpenCode Go again.")]
    InvalidAuth,
    #[error("OpenCode Go subscription required.")]
    GoSubscriptionRequired,
    #[error("Could not reach OpenCode Go. Check your internet connection.")]
    ConnectionFailed,
    #[error("OpenCode Go returned an invalid usage response.")]
    InvalidResponse,
    #[error("OpenCode Go usage request failed (HTTP {0}).")]
    RequestFailed(u16),
}

impl From<OpenCodeError> for ProviderError {
    fn from(error: OpenCodeError) -> Self {
        let kind = match error {
            OpenCodeError::InvalidAuth | OpenCodeError::NotDetected => {
                ProviderErrorKind::Authentication
            }
            OpenCodeError::GoSubscriptionRequired => ProviderErrorKind::Permission,
            OpenCodeError::CredentialsUnreadable => ProviderErrorKind::CredentialStorage,
            OpenCodeError::DataDirectoryUnreadable | OpenCodeError::DatabaseUnreadable => {
                ProviderErrorKind::LocalData
            }
            OpenCodeError::ConnectionFailed => ProviderErrorKind::Network,
            OpenCodeError::RequestFailed(429) => ProviderErrorKind::RateLimited,
            OpenCodeError::RequestFailed(500..=599) => ProviderErrorKind::Network,
            OpenCodeError::InvalidResponse | OpenCodeError::RequestFailed(_) => {
                ProviderErrorKind::InvalidResponse
            }
        };
        ProviderError::new(kind, error.to_string())
    }
}

pub struct OpenCodeProvider {
    paths: OpenCodePaths,
    scanner: OpenCodeUsageScanner,
    client: Result<OpenCodeClient, OpenCodeError>,
    pricing: Arc<PricingStore>,
    now: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>,
}

impl OpenCodeProvider {
    pub fn new(pricing: Arc<PricingStore>) -> Self {
        let paths = OpenCodePaths::new();
        Self {
            scanner: OpenCodeUsageScanner::new(paths.clone()),
            paths,
            client: OpenCodeClient::new(),
            pricing,
            now: Arc::new(Utc::now),
        }
    }

    #[cfg(test)]
    fn with_dependencies(
        paths: OpenCodePaths,
        client: OpenCodeClient,
        pricing: Arc<PricingStore>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            scanner: OpenCodeUsageScanner::new(paths.clone()),
            paths,
            client: Ok(client),
            pricing,
            now: Arc::new(move || now),
        }
    }

    fn refresh_snapshot(&self) -> Result<ProviderSnapshot, OpenCodeError> {
        let now = (self.now)();
        let Some(key) = self.paths.go_api_key()? else {
            if self.paths.database_files()?.is_empty() {
                return Err(OpenCodeError::NotDetected);
            }
            // No online source has been refreshed. Local success/failure determines availability.
            return Ok(snapshot(None, Vec::new(), Vec::new(), DateTime::UNIX_EPOCH));
        };
        let client = self.client.as_ref().map_err(|error| *error)?;
        let quotas = map_go_usage(client.fetch_go_usage(&key)?, now)?;
        Ok(snapshot(Some("Go".into()), quotas, Vec::new(), now))
    }
}

impl UsageProvider for OpenCodeProvider {
    fn definition(&self) -> ProviderDefinition {
        definition()
    }

    fn supports_local_usage(&self) -> bool {
        true
    }

    fn refresh_local_usage(&self) -> Option<Result<crate::providers::ScannedUsage, ProviderError>> {
        Some(
            self.scanner
                .scan((self.now)(), &self.pricing.current())
                .map(|scan| {
                    // A missing optional local source is normal for Go-only users.
                    scan.map(|scan| crate::providers::ScannedUsage {
                        history: scan.usage,
                        warnings: scan.warnings,
                    })
                    .unwrap_or_default()
                })
                .map_err(ProviderError::from),
        )
    }

    fn has_local_credentials(&self) -> bool {
        match self.paths.go_api_key() {
            Ok(Some(_)) | Err(OpenCodeError::CredentialsUnreadable) => true,
            Ok(None) | Err(_) => self.scanner.has_hosted_usage(),
        }
    }

    fn refresh(&self) -> Result<ProviderSnapshot, ProviderError> {
        self.refresh_snapshot().map_err(ProviderError::from)
    }
}

fn snapshot(
    plan: Option<String>,
    quotas: Vec<crate::models::QuotaWindow>,
    warnings: Vec<String>,
    refreshed_at: DateTime<Utc>,
) -> ProviderSnapshot {
    ProviderSnapshot {
        upstream_provider_id: None,
        accounts: None,
        provider_id: "opencode".into(),
        plan,
        quotas,
        value_metrics: Vec::new(),
        status_metrics: Vec::new(),
        notices: Vec::new(),
        usage_histories: UsageHistories::default(),
        warnings,
        refreshed_at,
    }
}

#[cfg(test)]
mod tests;
