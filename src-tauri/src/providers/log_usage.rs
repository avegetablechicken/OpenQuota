use std::{
    collections::HashSet,
    fs::{self, Metadata},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};

use chrono::{DateTime, Local, NaiveDate, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};

use crate::{
    models::UsageHistory,
    storage::{Storage, StorageError},
};

/// Cross-platform metadata key for one local log file. Nanosecond precision avoids stale cache hits
/// when sync tools or fast writers replace a same-sized file more than once inside one millisecond.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogFileFingerprint {
    pub size: u64,
    pub modified_nanos: i64,
}

impl LogFileFingerprint {
    pub fn from_metadata(metadata: &Metadata) -> Option<Self> {
        Some(Self {
            size: metadata.len(),
            modified_nanos: system_time_nanos(metadata.modified().ok()?),
        })
    }
}

/// How one Home client's log files are parsed. [`LocalLogs`] owns everything else about loading
/// them: the memo, the persistent per-file cache, the date window, pruning and skipped files.
pub(crate) trait LogFormat {
    type Event: Serialize + DeserializeOwned + Send + Sync;
    /// What a file's events depend on besides its content, such as the client it belongs to.
    type Context: PartialEq + Send + Sync;
    /// Names the client's rows in the persistent cache and its log messages.
    const PROVIDER_ID: &'static str;
    /// Bumped when parsed events change shape or meaning, so cached files are parsed again.
    const SCHEMA_VERSION: u8;
    /// A file that cannot be read fails the whole load instead of being skipped.
    const UNREADABLE_FAILS: bool = false;

    /// Reads one file. The persistent cache keeps the result per file, so it must depend on the
    /// file's content alone; anything else belongs in `attach`.
    fn parse(content: &str) -> Vec<Self::Event>;
    fn timestamp(event: &Self::Event) -> DateTime<Utc>;
    /// Applies the file's context to an event loaded from it.
    fn attach(_event: &mut Self::Event, _context: &Self::Context) {}
    /// Runs once over every loaded event in the window.
    fn deduplicate(events: Vec<Self::Event>) -> Vec<Self::Event> {
        events
    }
}

/// Identifies one load of a client's logs: the storage, the date window, and every file's state
/// and context.
#[derive(PartialEq, Eq)]
struct LoadKey<C> {
    storage: usize,
    since_date: NaiveDate,
    files: Vec<(PathBuf, Option<LogFileFingerprint>, C)>,
}

/// Loads one Home client's logs and reuses the result while none of its files changed.
///
/// Every card folds in the usage other clients routed to it, so a refresh loads each client's
/// logs once per card. The last result is kept, and its lock is held while loading, so concurrent
/// cards wait for one load instead of each decoding the same cache rows.
pub(crate) struct LocalLogs<F: LogFormat> {
    last: Mutex<Option<Loaded<F>>>,
}

/// A remembered load: its key and the events it produced.
type Loaded<F> = (
    LoadKey<<F as LogFormat>::Context>,
    Arc<Vec<<F as LogFormat>::Event>>,
);

impl<F: LogFormat> LocalLogs<F> {
    pub const fn new() -> Self {
        Self {
            last: Mutex::new(None),
        }
    }

    /// The events in `files` dated on or after `since_date`, deduplicated by the format. A
    /// repeated path is read once. A failed load, or one that skipped an unreadable file, is not
    /// remembered, so the next refresh retries it.
    pub fn load(
        &self,
        storage: &Storage,
        since_date: NaiveDate,
        files: impl IntoIterator<Item = (PathBuf, F::Context)>,
    ) -> Result<Arc<Vec<F::Event>>, LogCacheError> {
        let mut seen = HashSet::new();
        let files = files
            .into_iter()
            .filter(|(path, _)| seen.insert(path.clone()))
            .map(|(path, context)| {
                let fingerprint = fs::metadata(&path)
                    .ok()
                    .and_then(|metadata| LogFileFingerprint::from_metadata(&metadata));
                (path, fingerprint, context)
            })
            .collect();
        let key = LoadKey {
            storage: std::ptr::from_ref(storage) as usize,
            since_date,
            files,
        };
        // Every card shares the remembered events, so a hit copies nothing.
        let mut last = self
            .last
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached_key, events)) = last.as_ref() {
            if *cached_key == key {
                return Ok(Arc::clone(events));
            }
        }
        let (events, complete) = load_files::<F>(storage, &key)?;
        let events = Arc::new(events);
        *last = complete.then(|| (key, Arc::clone(&events)));
        Ok(events)
    }
}

/// Loads every file in `key`, prunes cache rows for files no longer there, and reports whether
/// every file could be read.
fn load_files<F: LogFormat>(
    storage: &Storage,
    key: &LoadKey<F::Context>,
) -> Result<(Vec<F::Event>, bool), LogCacheError> {
    let mut events = Vec::new();
    let mut complete = true;
    for (path, _, context) in &key.files {
        match load_file(storage, F::PROVIDER_ID, path, F::SCHEMA_VERSION, F::parse)? {
            FileEvents::Loaded(parsed) => events.extend(
                parsed
                    .into_iter()
                    .map(|mut event| {
                        F::attach(&mut event, context);
                        event
                    })
                    .filter(|event| {
                        F::timestamp(event).with_timezone(&Local).date_naive() >= key.since_date
                    }),
            ),
            FileEvents::Missing => {}
            FileEvents::Unreadable if F::UNREADABLE_FAILS => return Err(LogCacheError::Unreadable),
            FileEvents::Unreadable => complete = false,
        }
    }
    let seen_paths = key
        .files
        .iter()
        .map(|(path, ..)| path.clone())
        .collect::<HashSet<_>>();
    storage.prune_log_events(F::PROVIDER_ID, &seen_paths)?;
    Ok((F::deduplicate(events), complete))
}

#[derive(Debug, thiserror::Error)]
pub enum LogCacheError {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error("parsed local log events could not be cached")]
    Encode(#[from] serde_json::Error),
    #[error("a local log file could not be read")]
    Unreadable,
}

#[derive(Deserialize)]
struct CachedLogEvents<T> {
    schema_version: u8,
    events: Vec<T>,
}

#[derive(Serialize)]
struct CachedLogEventsRef<'a, T> {
    schema_version: u8,
    events: &'a [T],
}

/// What one file yielded.
#[derive(Debug, PartialEq)]
enum FileEvents<T> {
    Loaded(Vec<T>),
    /// The file is gone, as when a client deletes a session between discovery and loading.
    Missing,
    Unreadable,
}

fn unreadable<T>(
    storage: &Storage,
    provider_id: &str,
    path: &Path,
) -> Result<FileEvents<T>, LogCacheError> {
    crate::logging::local_usage_file_failed(provider_id, path);
    storage.remove_log_events(provider_id, path)?;
    Ok(FileEvents::Unreadable)
}

/// Loads one parsed log file from the persistent cache or refreshes it from disk. An unreadable file
/// is deliberately not an error for the whole provider and never becomes a cached empty result; its
/// stale cache row is removed so a later refresh retries the source file.
fn load_file<T>(
    storage: &Storage,
    provider_id: &str,
    path: &Path,
    schema_version: u8,
    parse: impl FnOnce(&str) -> Vec<T>,
) -> Result<FileEvents<T>, LogCacheError>
where
    T: Serialize + DeserializeOwned,
{
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            crate::logging::local_usage_file_recovered(provider_id, path);
            storage.remove_log_events(provider_id, path)?;
            return Ok(FileEvents::Missing);
        }
        Err(_) => return unreadable(storage, provider_id, path),
    };
    let Some(fingerprint) = LogFileFingerprint::from_metadata(&metadata) else {
        return unreadable(storage, provider_id, path);
    };
    if let Some(events) = storage
        .load_log_events(
            provider_id,
            path,
            fingerprint.size,
            fingerprint.modified_nanos,
        )?
        .as_deref()
        .and_then(|json| serde_json::from_str::<CachedLogEvents<T>>(json).ok())
        .filter(|cached| cached.schema_version == schema_version)
        .map(|cached| cached.events)
    {
        crate::logging::local_usage_file_recovered(provider_id, path);
        crate::app_debug!("cache", "local usage cache hit for {provider_id}");
        return Ok(FileEvents::Loaded(events));
    }

    let Ok(content) = fs::read_to_string(path) else {
        return unreadable(storage, provider_id, path);
    };
    crate::logging::local_usage_file_recovered(provider_id, path);
    crate::app_debug!("cache", "local usage cache miss for {provider_id}");
    let events = parse(&content);
    let json = serde_json::to_string(&CachedLogEventsRef {
        schema_version,
        events: &events,
    })?;
    storage.save_log_events(
        provider_id,
        path,
        fingerprint.size,
        fingerprint.modified_nanos,
        &json,
    )?;
    Ok(FileEvents::Loaded(events))
}

/// Keeps provider quotas usable when supplementary local history cannot be refreshed. A prior
/// persisted history is reused when available; otherwise the provider still returns its live data
/// with an explicit warning instead of failing the entire refresh.
pub fn scan_or_cached_usage<E>(
    storage: &Storage,
    provider_id: &str,
    identity: super::CacheIdentity<'_>,
    provider_name: &str,
    scan: impl FnOnce() -> Result<UsageHistory, E>,
    warnings: &mut Vec<String>,
) -> UsageHistory {
    match scan() {
        Ok(usage) => usage,
        Err(_) => {
            let tag = format!("plugin:{provider_id}");
            crate::app_warn!(
                &tag,
                "local usage history could not be refreshed; keeping live provider data"
            );
            warnings.push(format!(
                "Local {provider_name} usage history could not be refreshed; cached history is shown when available."
            ));
            storage
                .load_snapshot_for_identity(provider_id, identity)
                .ok()
                .flatten()
                .and_then(|snapshot| snapshot.usage_histories.local_device)
                .unwrap_or_default()
        }
    }
}

fn system_time_nanos(value: SystemTime) -> i64 {
    match value.duration_since(UNIX_EPOCH) {
        Ok(duration) => (duration.as_secs() as i64)
            .saturating_mul(1_000_000_000)
            .saturating_add(i64::from(duration.subsec_nanos())),
        Err(error) => {
            let duration = error.duration();
            (duration.as_secs() as i64)
                .saturating_mul(1_000_000_000)
                .saturating_add(i64::from(duration.subsec_nanos()))
                .saturating_neg()
        }
    }
}

/// Parses the timestamp variants emitted by provider CLIs on different operating systems and sync
/// paths. Calendar bucketing remains device-local after this returns a single absolute UTC instant.
pub fn parse_log_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    let mut value = raw.trim().to_owned();
    if value.is_empty() {
        return None;
    }
    if value.ends_with(" UTC") {
        value.truncate(value.len() - 4);
        value.push('Z');
    }
    if value.as_bytes().get(10) == Some(&b' ') {
        value.replace_range(10..11, "T");
    }
    normalize_fraction(&mut value);
    if value.len() >= 19 && !has_timezone(&value) {
        value.push('Z');
    }
    DateTime::parse_from_rfc3339(&value)
        .ok()
        .map(|timestamp| timestamp.to_utc())
}

fn normalize_fraction(value: &mut String) {
    if value.as_bytes().get(19) != Some(&b'.') {
        return;
    }
    let fraction_end = value[20..]
        .find(['Z', '+', '-'])
        .map(|index| index + 20)
        .unwrap_or(value.len());
    if fraction_end == 20
        || !value.as_bytes()[20..fraction_end]
            .iter()
            .all(u8::is_ascii_digit)
    {
        return;
    }
    let digits = fraction_end - 20;
    if digits > 3 {
        value.replace_range(23..fraction_end, "");
    } else if digits < 3 {
        value.insert_str(fraction_end, &"0".repeat(3 - digits));
    }
}

fn has_timezone(value: &str) -> bool {
    let suffix = value.get(19..).unwrap_or_default();
    suffix.ends_with('Z') || suffix.rfind(['+', '-']).is_some()
}

#[cfg(test)]
mod tests {
    use std::{cell::Cell, fs, path::PathBuf, sync::Arc};

    use chrono::{DateTime, NaiveDate, TimeZone, Utc};
    use serde::{Deserialize, Serialize};
    use tempfile::tempdir;

    use super::{
        load_file, parse_log_timestamp, scan_or_cached_usage, FileEvents, LocalLogs, LogCacheError,
        LogFileFingerprint, LogFormat,
    };
    use crate::{
        models::{DailyUsage, ProviderSnapshot, UsageHistory},
        storage::Storage,
    };

    fn cached_usage() -> UsageHistory {
        UsageHistory {
            daily: vec![DailyUsage {
                date: "2026-07-18".into(),
                tokens: 42,
                estimated_cost_usd: Some(0.5),
                estimate_complete: true,
            }],
            ..UsageHistory::default()
        }
    }

    fn codex_snapshot(usage: UsageHistory) -> ProviderSnapshot {
        ProviderSnapshot {
            upstream_provider_id: None,
            accounts: None,
            provider_id: "codex".into(),
            plan: None,
            quotas: Vec::new(),
            value_metrics: Vec::new(),
            status_metrics: Vec::new(),
            notices: Vec::new(),
            usage_histories: crate::models::UsageHistories::local_device(usage),
            warnings: Vec::new(),
            refreshed_at: Utc::now(),
        }
    }

    #[test]
    fn timestamp_variants_resolve_to_the_same_absolute_instant() {
        let expected = Utc.with_ymd_and_hms(2026, 7, 15, 9, 30, 45).unwrap()
            + chrono::Duration::milliseconds(123);
        for value in [
            "2026-07-15T09:30:45.123Z",
            " 2026-07-15 09:30:45.123456 UTC ",
            "2026-07-15T09:30:45.123",
            "2026-07-15T12:30:45.123+03:00",
            "2026-07-15T02:30:45.123-07:00",
        ] {
            assert_eq!(parse_log_timestamp(value), Some(expected), "value: {value}");
        }
    }

    #[test]
    fn timestamp_parser_rejects_non_timestamp_text() {
        assert_eq!(parse_log_timestamp(""), None);
        assert_eq!(parse_log_timestamp("tomorrow"), None);
    }

    #[test]
    fn failed_scan_keeps_cached_history_without_hiding_live_provider_data() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let cached_usage = cached_usage();
        storage
            .save_snapshot(&codex_snapshot(cached_usage.clone()))
            .unwrap();
        let mut warnings = Vec::new();

        let usage = scan_or_cached_usage(
            &storage,
            "codex",
            crate::providers::CacheIdentity::Unscoped,
            "Codex",
            || Err::<UsageHistory, ()>(()),
            &mut warnings,
        );

        assert_eq!(usage, cached_usage);
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("Codex"));
    }

    #[test]
    fn failed_scan_does_not_reuse_history_from_a_different_account() {
        let directory = tempdir().unwrap();
        let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
        let cached_usage = cached_usage();
        storage
            .save_snapshot_for_identity(&codex_snapshot(cached_usage.clone()), Some("account-a"))
            .unwrap();

        let mut warnings = Vec::new();
        let other_account = scan_or_cached_usage(
            &storage,
            "codex",
            crate::providers::CacheIdentity::Resolved("account-b"),
            "Codex",
            || Err::<UsageHistory, ()>(()),
            &mut warnings,
        );
        let same_account = scan_or_cached_usage(
            &storage,
            "codex",
            crate::providers::CacheIdentity::Resolved("account-a"),
            "Codex",
            || Err::<UsageHistory, ()>(()),
            &mut warnings,
        );
        let unresolved_account = scan_or_cached_usage(
            &storage,
            "codex",
            crate::providers::CacheIdentity::Unresolved,
            "Codex",
            || Err::<UsageHistory, ()>(()),
            &mut warnings,
        );

        assert_eq!(other_account, UsageHistory::default());
        assert_eq!(same_account, cached_usage);
        assert_eq!(unresolved_account, same_account);
    }

    thread_local! {
        static PARSES: Cell<usize> = const { Cell::new(0) };
    }

    fn parses() -> usize {
        PARSES.with(Cell::get)
    }

    /// One event per line: `<RFC 3339 timestamp> <text>`. The context is a tag `attach` copies.
    #[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
    struct Line {
        at: DateTime<Utc>,
        text: String,
        tag: u8,
    }

    struct Lines;

    impl LogFormat for Lines {
        type Event = Line;
        type Context = u8;
        const PROVIDER_ID: &'static str = "test";
        const SCHEMA_VERSION: u8 = 1;

        fn parse(content: &str) -> Vec<Line> {
            PARSES.with(|count| count.set(count.get() + 1));
            content
                .lines()
                .filter_map(|line| {
                    let (at, text) = line.split_once(' ')?;
                    Some(Line {
                        at: parse_log_timestamp(at)?,
                        text: text.to_owned(),
                        tag: 0,
                    })
                })
                .collect()
        }

        fn timestamp(event: &Line) -> DateTime<Utc> {
            event.at
        }

        fn attach(event: &mut Line, context: &u8) {
            event.tag = *context;
        }

        fn deduplicate(mut events: Vec<Line>) -> Vec<Line> {
            events.dedup_by(|a, b| a.text == b.text);
            events
        }
    }

    /// Lines, but a file that cannot be read fails the load.
    struct StrictLines;

    impl LogFormat for StrictLines {
        type Event = Line;
        type Context = u8;
        const PROVIDER_ID: &'static str = "strict";
        const SCHEMA_VERSION: u8 = 1;
        const UNREADABLE_FAILS: bool = true;

        fn parse(content: &str) -> Vec<Line> {
            Lines::parse(content)
        }

        fn timestamp(event: &Line) -> DateTime<Utc> {
            event.at
        }
    }

    const EARLY: &str = "2026-07-01T12:00:00Z";
    const LATE: &str = "2026-07-20T12:00:00Z";

    fn texts(events: &[Line]) -> Vec<&str> {
        events.iter().map(|event| event.text.as_str()).collect()
    }

    struct Fixture {
        directory: tempfile::TempDir,
        storage: Storage,
    }

    impl Fixture {
        fn new() -> Self {
            let directory = tempdir().unwrap();
            let storage = Storage::open(&directory.path().join("openquota.db")).unwrap();
            Self { directory, storage }
        }

        fn file(&self, name: &str, content: &[u8]) -> PathBuf {
            let path = self.directory.path().join(name);
            fs::write(&path, content).unwrap();
            path
        }

        fn cached(&self, provider_id: &str, path: &PathBuf) -> Option<String> {
            let fingerprint =
                LogFileFingerprint::from_metadata(&fs::metadata(path).unwrap()).unwrap();
            self.storage
                .load_log_events(
                    provider_id,
                    path,
                    fingerprint.size,
                    fingerprint.modified_nanos,
                )
                .unwrap()
        }
    }

    #[test]
    fn unchanged_logs_are_shared_and_changed_logs_reload() {
        let fixture = Fixture::new();
        let first = fixture.file("first.jsonl", format!("{LATE} a").as_bytes());
        let logs = LocalLogs::<Lines>::new();
        let before = parses();

        let loaded = logs
            .load(&fixture.storage, NaiveDate::MIN, [(first.clone(), 0)])
            .unwrap();
        assert_eq!(texts(&loaded), ["a"]);
        // A hit shares the remembered events instead of copying them.
        let reused = logs
            .load(&fixture.storage, NaiveDate::MIN, [(first.clone(), 0)])
            .unwrap();
        assert!(Arc::ptr_eq(&loaded, &reused));
        assert_eq!(parses(), before + 1);

        // A new memo still reads the unchanged file from the persistent cache.
        let fresh = LocalLogs::<Lines>::new()
            .load(&fixture.storage, NaiveDate::MIN, [(first.clone(), 0)])
            .unwrap();
        assert_eq!(texts(&fresh), ["a"]);
        assert_eq!(parses(), before + 1);

        fs::write(&first, format!("{LATE} bc")).unwrap();
        let changed = logs
            .load(&fixture.storage, NaiveDate::MIN, [(first.clone(), 0)])
            .unwrap();
        assert_eq!(texts(&changed), ["bc"]);
        assert_eq!(parses(), before + 2);
    }

    #[test]
    fn a_new_schema_version_parses_cached_files_again() {
        struct LinesV2;
        impl LogFormat for LinesV2 {
            type Event = Line;
            type Context = u8;
            const PROVIDER_ID: &'static str = "test";
            const SCHEMA_VERSION: u8 = 2;
            fn parse(content: &str) -> Vec<Line> {
                Lines::parse(content)
            }
            fn timestamp(event: &Line) -> DateTime<Utc> {
                event.at
            }
        }
        let fixture = Fixture::new();
        let path = fixture.file("session.jsonl", format!("{LATE} a").as_bytes());
        let before = parses();
        LocalLogs::<Lines>::new()
            .load(&fixture.storage, NaiveDate::MIN, [(path.clone(), 0)])
            .unwrap();
        LocalLogs::<LinesV2>::new()
            .load(&fixture.storage, NaiveDate::MIN, [(path, 0)])
            .unwrap();
        assert_eq!(parses(), before + 2);
    }

    #[test]
    fn each_files_context_is_attached_and_a_changed_context_reloads() {
        let fixture = Fixture::new();
        let first = fixture.file("first.jsonl", format!("{LATE} a").as_bytes());
        let second = fixture.file("second.jsonl", format!("{LATE} b").as_bytes());
        let logs = LocalLogs::<Lines>::new();

        let loaded = logs
            .load(
                &fixture.storage,
                NaiveDate::MIN,
                [(first.clone(), 1), (second.clone(), 2)],
            )
            .unwrap();
        assert_eq!(
            loaded.iter().map(|event| event.tag).collect::<Vec<_>>(),
            [1, 2]
        );
        let moved = logs
            .load(&fixture.storage, NaiveDate::MIN, [(first, 1), (second, 3)])
            .unwrap();
        assert!(!Arc::ptr_eq(&loaded, &moved));
        assert_eq!(moved[1].tag, 3);
    }

    #[test]
    fn the_load_keeps_the_window_reads_a_repeated_path_once_and_deduplicates() {
        let fixture = Fixture::new();
        let path = fixture.file(
            "session.jsonl",
            format!("{EARLY} old\n{LATE} new\n{LATE} new").as_bytes(),
        );
        let since = NaiveDate::from_ymd_opt(2026, 7, 10).unwrap();
        let loaded = LocalLogs::<Lines>::new()
            .load(&fixture.storage, since, [(path.clone(), 0), (path, 0)])
            .unwrap();
        assert_eq!(texts(&loaded), ["new"]);
    }

    #[test]
    fn rows_for_files_no_longer_found_are_pruned() {
        let fixture = Fixture::new();
        let kept = fixture.file("kept.jsonl", format!("{LATE} a").as_bytes());
        let gone = fixture.file("gone.jsonl", format!("{LATE} b").as_bytes());
        let logs = LocalLogs::<Lines>::new();
        logs.load(
            &fixture.storage,
            NaiveDate::MIN,
            [(kept.clone(), 0), (gone.clone(), 0)],
        )
        .unwrap();
        assert!(fixture.cached("test", &gone).is_some());

        logs.load(&fixture.storage, NaiveDate::MIN, [(kept.clone(), 0)])
            .unwrap();
        assert!(fixture.cached("test", &gone).is_none());
        assert!(fixture.cached("test", &kept).is_some());
    }

    #[test]
    fn an_unreadable_file_is_skipped_retried_and_never_cached_empty() {
        let fixture = Fixture::new();
        let good = fixture.file("good.jsonl", format!("{LATE} a").as_bytes());
        let bad = fixture.file("bad.jsonl", &[0xff, 0xfe]);
        let fingerprint = LogFileFingerprint::from_metadata(&fs::metadata(&bad).unwrap()).unwrap();
        fixture
            .storage
            .save_log_events(
                "test",
                &bad,
                fingerprint.size,
                fingerprint.modified_nanos.saturating_sub(1),
                r#"{"schema_version":1,"events":[]}"#,
            )
            .unwrap();
        let files = || [(good.clone(), 0), (bad.clone(), 0)];
        let logs = LocalLogs::<Lines>::new();

        let skipped = logs
            .load(&fixture.storage, NaiveDate::MIN, files())
            .unwrap();
        assert_eq!(texts(&skipped), ["a"]);
        assert_eq!(
            fixture
                .storage
                .load_log_events(
                    "test",
                    &bad,
                    fingerprint.size,
                    fingerprint.modified_nanos.saturating_sub(1),
                )
                .unwrap(),
            None
        );
        // Nothing changed on disk, but a load that skipped a file is not remembered.
        let retried = logs
            .load(&fixture.storage, NaiveDate::MIN, files())
            .unwrap();
        assert!(!Arc::ptr_eq(&skipped, &retried));

        fs::write(&bad, format!("{LATE} b")).unwrap();
        let recovered = logs
            .load(&fixture.storage, NaiveDate::MIN, files())
            .unwrap();
        assert_eq!(texts(&recovered), ["a", "b"]);
        let remembered = logs
            .load(&fixture.storage, NaiveDate::MIN, files())
            .unwrap();
        assert!(Arc::ptr_eq(&recovered, &remembered));
    }

    #[test]
    fn a_strict_format_fails_on_an_unreadable_file_but_not_a_missing_one() {
        let fixture = Fixture::new();
        let directory = fixture.directory.path().join("not-a-file");
        fs::create_dir(&directory).unwrap();
        let logs = LocalLogs::<StrictLines>::new();
        assert!(matches!(
            logs.load(&fixture.storage, NaiveDate::MIN, [(directory, 0)]),
            Err(LogCacheError::Unreadable)
        ));
        let missing = fixture.directory.path().join("missing.jsonl");
        assert!(logs
            .load(&fixture.storage, NaiveDate::MIN, [(missing, 0)])
            .unwrap()
            .is_empty());
    }

    #[test]
    fn a_file_that_disappears_is_missing_rather_than_unreadable() {
        let fixture = Fixture::new();
        let path = fixture.file("session.jsonl", format!("{LATE} a").as_bytes());
        assert_eq!(
            load_file(&fixture.storage, "test", &path, 1, |_| vec![1u8]).unwrap(),
            FileEvents::Loaded(vec![1])
        );
        let missing = fixture.directory.path().join("missing.jsonl");
        assert_eq!(
            load_file(&fixture.storage, "test", &missing, 1, |_| vec![1u8]).unwrap(),
            FileEvents::Missing
        );
    }
}
