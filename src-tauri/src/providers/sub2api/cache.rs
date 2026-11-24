use std::path::Path;

use chrono::{Days, NaiveDate};
use rusqlite::{params, Connection, OpenFlags, OptionalExtension};

use super::Sub2ApiError;

/// Pages a requested 30-day backfill may download during one refresh.
const BACKFILL_PAGES_PER_REFRESH: usize = 16;

/// One database per connection slot, shared by both billing modes. Persist only the
/// typed pricing inputs and statistics, never the server's full administrative DTOs.
pub(super) struct UsageCache {
    pub(super) db: Connection,
    pages_left: usize,
    backfill: bool,
    backfill_incomplete: bool,
}

/// What the local cache reports about Upstream pricing coverage, without network access.
pub(super) struct HistoryCoverage {
    pub(super) priced_from: Option<NaiveDate>,
    pub(super) backfill_pending: bool,
}

impl From<rusqlite::Error> for Sub2ApiError {
    fn from(_: rusqlite::Error) -> Self {
        Self::HistoryCache
    }
}

impl UsageCache {
    pub(super) fn open(path: &Path, scope: &str) -> Result<Self, Sub2ApiError> {
        let mut db = Connection::open(path)?;
        let tx = db.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS identity(scope TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS records(account INTEGER NOT NULL,id INTEGER NOT NULL,
                 day TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(account,id));
             CREATE TABLE IF NOT EXISTS synced(account INTEGER PRIMARY KEY, activity TEXT NOT NULL, start TEXT NOT NULL, end TEXT NOT NULL, checked INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS records_day ON records(day);
             CREATE TABLE IF NOT EXISTS stats(account INTEGER PRIMARY KEY,payload TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS coverage(account INTEGER PRIMARY KEY,start TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS meta(key TEXT PRIMARY KEY,value TEXT NOT NULL);",
        )?;
        let old: Option<String> = tx
            .query_row("SELECT scope FROM identity LIMIT 1", [], |r| r.get(0))
            .optional()?;
        if old.as_deref() != Some(scope) {
            tx.execute_batch(
                "DELETE FROM records; DELETE FROM stats; DELETE FROM synced; DELETE FROM identity;
                 DELETE FROM coverage; DELETE FROM meta;",
            )?;
            tx.execute("INSERT INTO identity VALUES(?1)", [scope])?;
        }
        let backfill = tx
            .query_row("SELECT 1 FROM meta WHERE key='backfill'", [], |_| Ok(()))
            .optional()?
            .is_some();
        tx.commit()?;
        Ok(Self {
            db,
            pages_left: BACKFILL_PAGES_PER_REFRESH,
            backfill,
            backfill_incomplete: false,
        })
    }

    /// Reads coverage for the settings view. A missing or foreign cache reports nothing.
    pub(super) fn coverage_summary(
        path: &Path,
        scope: &str,
        since: NaiveDate,
    ) -> Result<HistoryCoverage, Sub2ApiError> {
        let empty = HistoryCoverage {
            priced_from: None,
            backfill_pending: false,
        };
        let db = match Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY) {
            Ok(db) => db,
            Err(_) => return Ok(empty),
        };
        let stored: Option<String> = match db
            .query_row("SELECT scope FROM identity LIMIT 1", [], |r| r.get(0))
            .optional()
        {
            Ok(scope) => scope,
            // An older cache without these tables has no coverage yet.
            Err(_) => return Ok(empty),
        };
        if stored.as_deref() != Some(scope) {
            return Ok(empty);
        }
        let latest: Option<String> = db
            .query_row("SELECT max(start) FROM coverage", [], |r| r.get(0))
            .optional()
            .ok()
            .flatten()
            .flatten();
        let backfill = db
            .query_row("SELECT 1 FROM meta WHERE key='backfill'", [], |_| Ok(()))
            .optional()
            .ok()
            .flatten()
            .is_some();
        let priced_from = latest
            .as_deref()
            .and_then(|day| day.parse::<NaiveDate>().ok())
            .map(|day| day.max(since));
        Ok(HistoryCoverage {
            priced_from,
            backfill_pending: backfill && priced_from.is_some_and(|day| day > since),
        })
    }

    pub(super) fn request_backfill(path: &Path, scope: &str) -> Result<(), Sub2ApiError> {
        let cache = Self::open(path, scope)?;
        cache.db.execute(
            "INSERT OR REPLACE INTO meta(key,value) VALUES('backfill','1')",
            [],
        )?;
        Ok(())
    }

    pub(super) fn backfill_requested(&self) -> bool {
        self.backfill
    }

    /// The local date from which an account's request records are fully synced. A new
    /// account starts today instead of downloading the whole 30-day window; an account
    /// that an earlier version had already synced completely keeps its full window.
    pub(super) fn coverage(
        &self,
        account: i64,
        since: NaiveDate,
        today: NaiveDate,
    ) -> Result<NaiveDate, Sub2ApiError> {
        let stored: Option<String> = self
            .db
            .query_row(
                "SELECT start FROM coverage WHERE account=?1",
                [account],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(day) = stored.and_then(|day| day.parse::<NaiveDate>().ok()) {
            return Ok(day.max(since));
        }
        let fully_synced = self
            .db
            .query_row("SELECT 1 FROM synced WHERE account=?1", [account], |_| {
                Ok(())
            })
            .optional()?
            .is_some();
        let start = if fully_synced { since } else { today };
        self.set_coverage(account, start)?;
        Ok(start)
    }

    pub(super) fn set_coverage(&self, account: i64, start: NaiveDate) -> Result<(), Sub2ApiError> {
        self.db.execute(
            "INSERT OR REPLACE INTO coverage(account,start) VALUES(?1,?2)",
            params![account, start.to_string()],
        )?;
        Ok(())
    }

    pub(super) fn mark_backfill_incomplete(&mut self) {
        self.backfill_incomplete = true;
    }

    /// Clears a requested backfill once every account in this refresh reached the window start.
    pub(super) fn finish_backfill(&self) -> Result<(), Sub2ApiError> {
        if self.backfill && !self.backfill_incomplete {
            self.db
                .execute("DELETE FROM meta WHERE key='backfill'", [])?;
        }
        Ok(())
    }

    pub(super) fn take_page(&mut self) -> Result<(), Sub2ApiError> {
        self.pages_left = self
            .pages_left
            .checked_sub(1)
            .ok_or(Sub2ApiError::HistorySyncPending)?;
        Ok(())
    }
}

/// The UTC calendar date containing local midnight of `day`; records on or after that
/// instant are found from this UTC date onward.
pub(super) fn utc_start(day: NaiveDate) -> NaiveDate {
    day.and_hms_opt(0, 0, 0)
        .and_then(|midnight| midnight.and_local_timezone(chrono::Local).earliest())
        .map(|midnight| midnight.with_timezone(&chrono::Utc).date_naive())
        .unwrap_or_else(|| day.checked_sub_days(Days::new(1)).unwrap_or(day))
}
