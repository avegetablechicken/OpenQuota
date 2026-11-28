use std::{collections::HashMap, path::PathBuf};

use chrono::{DateTime, Utc};

use crate::{
    models::UsageHistory,
    pricing::{ModelPricing, TokenBreakdown},
    providers::local_usage::{
        card_usage::DailyUsageAccumulator,
        card_usage::{self, Settled, UsageObservation},
    },
};

use super::{
    database::{has_hosted_usage, read_database, DatabaseRead},
    paths::OpenCodePaths,
    record::{CostProvenance, UsageRecord},
    OpenCodeError,
};

const SCAN_DAYS: i64 = 33;
pub(crate) const USAGE_SOURCE_NOTE: &str =
    "From your OpenCode local database; missing costs use catalog estimates";

#[derive(Debug)]
pub(crate) struct OpenCodeUsageScan {
    pub(crate) usage: UsageHistory,
    pub(crate) warnings: Vec<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct OpenCodeUsageScanner {
    paths: OpenCodePaths,
}

impl OpenCodeUsageScanner {
    pub(crate) fn new(paths: OpenCodePaths) -> Self {
        Self { paths }
    }

    #[cfg(test)]
    pub(crate) fn for_paths(paths: Vec<PathBuf>) -> Self {
        let data_directory = paths
            .first()
            .and_then(|path| path.parent())
            .unwrap_or_else(|| std::path::Path::new("."))
            .to_path_buf();
        Self::new(OpenCodePaths::for_data_directory(data_directory))
    }

    pub(crate) fn scan(
        &self,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<Option<OpenCodeUsageScan>, OpenCodeError> {
        let paths = self.paths.database_files()?;
        self.scan_paths(paths, now, pricing)
    }

    pub(crate) fn scan_paths(
        &self,
        mut paths: Vec<PathBuf>,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<Option<OpenCodeUsageScan>, OpenCodeError> {
        paths.sort();
        paths.dedup();
        self.scan_sorted_paths(paths, now, pricing)
    }

    fn scan_sorted_paths(
        &self,
        paths: Vec<PathBuf>,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
    ) -> Result<Option<OpenCodeUsageScan>, OpenCodeError> {
        let Some(scan) = scan_records(paths, now, pricing, "opencode")? else {
            return Ok(None);
        };
        Ok(Some(OpenCodeUsageScan {
            usage: aggregate_history(&scan.records, now, pricing),
            warnings: scan.warnings,
        }))
    }

    pub(crate) fn scan_into(
        &self,
        now: DateTime<Utc>,
        pricing: &ModelPricing,
        card_id: &str,
        accumulator: &mut DailyUsageAccumulator,
    ) -> Result<bool, OpenCodeError> {
        let paths = self.paths.database_files()?;
        let Some(scan) = scan_records(paths, now, pricing, card_id)? else {
            return Ok(false);
        };
        Ok(aggregate_into(&scan.records, now, pricing, accumulator))
    }

    pub(crate) fn has_hosted_usage(&self) -> bool {
        let paths = match self.paths.database_files() {
            Ok(paths) => paths,
            Err(_) => {
                crate::app_warn!(
                    "plugin:opencode",
                    "usage probe could not enumerate the local data directory"
                );
                return true;
            }
        };
        paths.iter().any(|path| match has_hosted_usage(path) {
            Ok(found) => found,
            Err(()) => {
                crate::app_warn!(
                    "plugin:opencode",
                    "usage probe could not read one local database"
                );
                false
            }
        })
    }
}

struct OpenCodeRecordScan {
    records: Vec<UsageRecord>,
    warnings: Vec<String>,
}

fn scan_records(
    mut paths: Vec<PathBuf>,
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    card_id: &str,
) -> Result<Option<OpenCodeRecordScan>, OpenCodeError> {
    paths.sort();
    paths.dedup();
    if paths.is_empty() {
        return Ok(None);
    }

    let cutoff_ms = (now - chrono::Duration::days(SCAN_DAYS)).timestamp_millis();
    let mut records = Vec::new();
    let mut usable_databases = 0_usize;
    let mut failed_databases = 0_usize;
    for path in paths {
        match read_database(&path, cutoff_ms, pricing, card_id) {
            Ok(DatabaseRead::Missing) => {}
            Ok(DatabaseRead::Usable(database)) => {
                usable_databases += 1;
                records.extend(database.records);
            }
            Err(()) => failed_databases += 1,
        }
    }
    if usable_databases == 0 {
        return if failed_databases == 0 {
            Ok(None)
        } else {
            Err(OpenCodeError::DatabaseUnreadable)
        };
    }

    let records = deduplicate(records);
    let mut warnings = Vec::new();
    if failed_databases > 0 {
        crate::app_warn!(
            "plugin:opencode",
            "{failed_databases} local database(s) could not be read; usable sources remain"
        );
        warnings.push(
            "Some OpenCode databases could not be read; available local usage is shown.".into(),
        );
    }

    Ok(Some(OpenCodeRecordScan { records, warnings }))
}

fn deduplicate(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    let mut deduplicated = HashMap::<(String, String), UsageRecord>::new();
    for candidate in records {
        match deduplicated.entry(candidate.key.clone()) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(candidate);
            }
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                if better_record(&candidate, entry.get()) {
                    entry.insert(candidate);
                }
            }
        }
    }
    let mut records = deduplicated.into_values().collect::<Vec<_>>();
    records.sort_by(|left, right| {
        left.timestamp
            .cmp(&right.timestamp)
            .then_with(|| left.key.cmp(&right.key))
    });
    records
}

fn better_record(candidate: &UsageRecord, current: &UsageRecord) -> bool {
    let quality = |record: &UsageRecord| {
        (
            record.cost.is_some() && !record.incomplete_cost,
            record.cost.is_some(),
            record.tokens,
            record.cost_provenance == CostProvenance::Exact,
        )
    };
    quality(candidate) > quality(current)
        || (quality(candidate) == quality(current)
            && candidate.cost.unwrap_or_default() > current.cost.unwrap_or_default())
}

fn aggregate_history(
    records: &[UsageRecord],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
) -> UsageHistory {
    let mut accumulator = DailyUsageAccumulator::default();
    aggregate_into(records, now, pricing, &mut accumulator);
    accumulator.build(now, USAGE_SOURCE_NOTE)
}

/// OpenCode settles each message's Card and cost at parse, so only the Scan window applies here.
fn aggregate_into(
    records: &[UsageRecord],
    now: DateTime<Utc>,
    pricing: &ModelPricing,
    accumulator: &mut DailyUsageAccumulator,
) -> bool {
    let observations = records.iter().map(|record| {
        let observation = UsageObservation {
            total_tokens: record.tokens,
            ..UsageObservation::new(
                record.timestamp,
                Some(&record.model),
                TokenBreakdown::default(),
                None,
            )
        };
        let settled = Settled {
            others: record.is_other,
            cost: record.cost,
            estimated: record.cost_provenance == CostProvenance::Estimated,
            priced_tokens: record.priced_tokens,
            complete: !record.incomplete_cost,
            unpriced_marker_only: false,
        };
        (observation, settled)
    });
    card_usage::record_settled(observations, now, true, pricing, accumulator).own_series
}

#[cfg(test)]
mod unit_tests {
    use chrono::{TimeZone, Utc};

    use super::{aggregate_history, deduplicate};
    use crate::providers::opencode::record::{CostProvenance, UsageRecord};

    fn record(tokens: u64, cost: Option<f64>, exact: bool) -> UsageRecord {
        UsageRecord {
            key: ("session".into(), "message".into()),
            timestamp: Utc.with_ymd_and_hms(2026, 7, 18, 10, 0, 0).unwrap(),
            model: "model".into(),
            tokens,
            cost,
            cost_provenance: if exact {
                CostProvenance::Exact
            } else {
                CostProvenance::Estimated
            },
            incomplete_cost: cost.is_none(),
            priced_tokens: if cost.is_some() { tokens } else { 0 },
            is_other: false,
        }
    }

    #[test]
    fn duplicate_messages_choose_the_most_complete_deterministically() {
        let records = deduplicate(vec![
            record(100, Some(1.0), false),
            record(200, Some(2.0), true),
            record(300, None, false),
        ]);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tokens, 200);
        assert_eq!(records[0].cost, Some(2.0));
        assert_eq!(records[0].cost_provenance, CostProvenance::Exact);
    }

    #[test]
    fn exact_and_estimated_costs_keep_their_period_provenance() {
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 12, 0, 0).unwrap();
        let exact = aggregate_history(
            &[record(100, Some(1.0), true)],
            now,
            &crate::pricing::test_bundled_pricing(),
        );
        assert!(!exact.today.unwrap().cost_estimated);

        let estimated = aggregate_history(
            &[record(100, Some(1.0), false)],
            now,
            &crate::pricing::test_bundled_pricing(),
        );
        assert!(estimated.today.unwrap().cost_estimated);
    }

    #[test]
    fn partially_priced_messages_leave_their_model_cost_incomplete() {
        let now = Utc.with_ymd_and_hms(2026, 7, 18, 12, 0, 0).unwrap();
        let mut partial = record(1_000, Some(0.1), true);
        partial.incomplete_cost = true;
        partial.priced_tokens = 100;
        let mut priced = record(100, Some(1.0), true);
        priced.key.1 = "priced".into();
        priced.model = "priced-model".into();

        let today = aggregate_history(
            &[partial, priced],
            now,
            &crate::pricing::test_bundled_pricing(),
        )
        .today
        .unwrap();
        assert_eq!(today.tokens, 1_100);
        assert!((today.estimated_cost_usd.unwrap() - 1.1).abs() < 1e-9);
        assert_eq!(today.unknown_models, ["model"]);
        let models = today.model_breakdown.unwrap().models;
        let cost = |name: &str| {
            models
                .iter()
                .find(|entry| entry.model == name)
                .map(|entry| entry.cost_usd)
                .unwrap()
        };
        assert_eq!(cost("model"), None);
        assert_eq!(cost("priced-model"), Some(1.0));
    }

    /// Pins how opencode's settled records reach a Card, so moving the accumulator calls into
    /// `card_usage` cannot change what any Card shows.
    mod characterization {
        use chrono::{DateTime, Duration, TimeZone, Utc};

        use super::super::aggregate_into;
        use crate::providers::{
            local_usage::card_usage::{
                test_support::{assert_pinned, relative_json},
                DailyUsageAccumulator,
            },
            opencode::record::{CostProvenance, UsageRecord},
        };

        fn now() -> DateTime<Utc> {
            Utc.with_ymd_and_hms(2026, 7, 15, 12, 0, 0).unwrap()
        }

        #[allow(clippy::too_many_arguments)]
        fn record(
            days_ago: i64,
            model: &str,
            tokens: u64,
            cost: Option<f64>,
            exact: bool,
            incomplete: bool,
            priced_tokens: u64,
            is_other: bool,
        ) -> UsageRecord {
            UsageRecord {
                key: ("session".into(), format!("{model}-{days_ago}-{tokens}")),
                timestamp: now() - Duration::days(days_ago),
                model: model.into(),
                tokens,
                cost,
                cost_provenance: if exact {
                    CostProvenance::Exact
                } else {
                    CostProvenance::Estimated
                },
                incomplete_cost: incomplete,
                priced_tokens,
                is_other,
            }
        }

        fn records() -> Vec<UsageRecord> {
            let mut future = record(
                0,
                "claude-sonnet-4-5",
                9_999,
                Some(9.0),
                true,
                false,
                9_999,
                false,
            );
            future.timestamp = now() + Duration::hours(1);
            vec![
                // Own series: exact, estimated, partially priced, unpriced, unknown marker.
                record(
                    0,
                    "claude-sonnet-4-5",
                    1_000,
                    Some(0.4),
                    true,
                    false,
                    1_000,
                    false,
                ),
                record(
                    0,
                    "claude-sonnet-4-5",
                    500,
                    Some(0.0),
                    true,
                    false,
                    500,
                    false,
                ),
                record(
                    0,
                    "claude-opus-4-6",
                    2_000,
                    Some(0.3),
                    false,
                    false,
                    2_000,
                    false,
                ),
                record(
                    1,
                    "claude-opus-4-6",
                    3_000,
                    Some(0.2),
                    false,
                    true,
                    1_000,
                    false,
                ),
                record(1, "claude-opus-4-6", 3_000, Some(0.1), true, true, 0, false),
                record(2, "claude-unreleased", 700, None, false, true, 0, false),
                record(2, "Unattributed", 400, None, false, true, 0, false),
                record(2, "claude-ghost", 0, None, false, true, 0, false),
                record(2, "claude-empty", 0, None, false, false, 0, false),
                // Others: exact, estimated, partially priced (c575c43), unpriced, empty.
                record(4, "deepseek-chat", 900, Some(0.05), true, false, 900, true),
                record(4, "deepseek-chat", 900, Some(0.04), false, false, 900, true),
                record(4, "qwen3-coder", 1_200, Some(0.03), false, true, 400, true),
                record(4, "totally-unknown-model", 50, None, false, true, 0, true),
                record(4, "local-llama", 0, None, false, true, 0, true),
                // Zero tokens with a cost.
                record(5, "claude-sonnet-4-5", 0, Some(0.2), true, false, 0, false),
                // Window edges: today-29 counts, today-30 does not.
                record(
                    29,
                    "claude-sonnet-4-5",
                    100,
                    Some(0.01),
                    false,
                    false,
                    100,
                    false,
                ),
                record(
                    30,
                    "claude-sonnet-4-5",
                    100,
                    Some(0.01),
                    false,
                    false,
                    100,
                    false,
                ),
                record(
                    30,
                    "deepseek-chat",
                    100,
                    Some(0.01),
                    false,
                    false,
                    100,
                    true,
                ),
                record(
                    31,
                    "claude-sonnet-4-5",
                    100,
                    Some(0.01),
                    false,
                    false,
                    100,
                    false,
                ),
                future,
            ]
        }

        #[test]
        fn settled_records_are_pinned() {
            let mut accumulator = DailyUsageAccumulator::default();
            let contributed = aggregate_into(
                &records(),
                now(),
                &crate::pricing::test_bundled_pricing(),
                &mut accumulator,
            );
            let json = relative_json(&accumulator.build(now(), "test"), now());
            assert_pinned(
                "settled",
                &format!("contributed={contributed}\n{json}"),
                SETTLED,
            );
        }

        #[test]
        fn only_others_or_nothing_does_not_contribute() {
            let others = [record(
                0,
                "deepseek-chat",
                900,
                Some(0.05),
                true,
                false,
                900,
                true,
            )];
            let mut accumulator = DailyUsageAccumulator::default();
            assert!(!aggregate_into(
                &others,
                now(),
                &crate::pricing::test_bundled_pricing(),
                &mut accumulator
            ));
            let unknown = [record(0, "claude-ghost", 0, None, false, true, 0, false)];
            let mut accumulator = DailyUsageAccumulator::default();
            assert!(aggregate_into(
                &unknown,
                now(),
                &crate::pricing::test_bundled_pricing(),
                &mut accumulator
            ));
            let empty = [record(0, "claude-empty", 0, None, false, false, 0, false)];
            let mut accumulator = DailyUsageAccumulator::default();
            assert!(!aggregate_into(
                &empty,
                now(),
                &crate::pricing::test_bundled_pricing(),
                &mut accumulator
            ));
        }

        const SETTLED: &str = r#"
contributed=true
{
  "today": {
    "tokens": 3500,
    "estimatedCostUsd": 0.7,
    "costEstimated": true,
    "estimateComplete": true,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1500,
          "costUsd": 0.4
        },
        {
          "model": "claude-opus-4-6",
          "totalTokens": 2000,
          "costUsd": 0.3
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": []
  },
  "yesterday": {
    "tokens": 6000,
    "estimatedCostUsd": 0.30000000000000004,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-opus-4-6",
          "totalTokens": 6000,
          "costUsd": null
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "claude-opus-4-6"
    ]
  },
  "last30Days": {
    "tokens": 10700,
    "estimatedCostUsd": 1.21,
    "costEstimated": true,
    "estimateComplete": false,
    "modelBreakdown": {
      "models": [
        {
          "model": "claude-sonnet-4-5",
          "totalTokens": 1600,
          "costUsd": 0.6100000000000001
        },
        {
          "model": "claude-opus-4-6",
          "totalTokens": 8000,
          "costUsd": null
        },
        {
          "model": "claude-unreleased",
          "totalTokens": 700,
          "costUsd": null
        },
        {
          "model": "Other",
          "totalTokens": 400,
          "costUsd": null,
          "variants": [
            {
              "model": "Unattributed",
              "totalTokens": 400,
              "costUsd": null
            }
          ]
        }
      ],
      "sourceNote": "test"
    },
    "unknownModels": [
      "Unattributed",
      "claude-ghost",
      "claude-opus-4-6",
      "claude-unreleased"
    ]
  },
  "daily": [
    {
      "date": "today-0",
      "tokens": 3500,
      "estimatedCostUsd": 0.7,
      "estimateComplete": true
    },
    {
      "date": "today-1",
      "tokens": 6000,
      "estimatedCostUsd": 0.30000000000000004,
      "estimateComplete": false
    },
    {
      "date": "today-2",
      "tokens": 1100,
      "estimatedCostUsd": null,
      "estimateComplete": false
    },
    {
      "date": "today-5",
      "tokens": 0,
      "estimatedCostUsd": 0.2,
      "estimateComplete": true
    },
    {
      "date": "today-29",
      "tokens": 100,
      "estimatedCostUsd": 0.01,
      "estimateComplete": true
    }
  ],
  "unknownModels": [
    "Unattributed",
    "claude-ghost",
    "claude-opus-4-6",
    "claude-unreleased"
  ],
  "otherUsage": {
    "today": null,
    "yesterday": null,
    "last30Days": {
      "models": {
        "deepseek-chat": {
          "tokens": 1800,
          "pricedTokens": 1800,
          "estimatedCostUsd": 0.09,
          "costEstimated": true
        },
        "qwen3-coder": {
          "tokens": 1200,
          "pricedTokens": 400,
          "estimatedCostUsd": 0.03,
          "costEstimated": true
        },
        "totally-unknown-model": {
          "tokens": 50,
          "pricedTokens": 0,
          "estimatedCostUsd": null,
          "costEstimated": false
        }
      },
      "tokens": 3050,
      "pricedTokens": 2200,
      "estimatedCostUsd": 0.12,
      "costEstimated": true,
      "estimateComplete": false
    }
  }
}
"#;
    }
}
