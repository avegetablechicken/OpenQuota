use std::path::Path;

use chrono::{Days, NaiveDate};
use rusqlite::{Connection, OptionalExtension};

use super::{Sub2ApiError, Sub2ApiUpstream};
use crate::hashing::sha256_hex;

const HISTORY_PAGES_PER_REFRESH: usize = 16;

pub(super) struct UsageCache {
    pub(super) db: Connection,
    pages_left: usize,
}

impl From<rusqlite::Error> for Sub2ApiError {
    fn from(_: rusqlite::Error) -> Self {
        Self::HistoryCache
    }
}

impl UsageCache {
    // Sub2API account IDs are per instance. Changing the tracked platform only
    // selects which accounts to read; their cached request records stay independent.
    pub(super) fn for_connection(
        path: &Path,
        base_url: &str,
        email: &str,
    ) -> Result<Self, Sub2ApiError> {
        let connection = format!("{base_url}\0{email}");
        let scope = sha256_hex(connection.as_bytes());
        let legacy = [Sub2ApiUpstream::Codex, Sub2ApiUpstream::Claude]
            .map(|upstream| sha256_hex(format!("{connection}\0{upstream}").as_bytes()));
        Self::open_scoped(path, &scope, &legacy)
    }

    #[cfg(test)]
    pub(super) fn open(path: &Path, scope: &str) -> Result<Self, Sub2ApiError> {
        Self::open_scoped(path, scope, &[])
    }

    fn open_scoped(path: &Path, scope: &str, legacy: &[String]) -> Result<Self, Sub2ApiError> {
        let mut db = Connection::open(path)?;
        let tx = db.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS identity(scope TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS records(account INTEGER NOT NULL,id INTEGER NOT NULL,
                 day TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(account,id));
             CREATE TABLE IF NOT EXISTS synced(account INTEGER PRIMARY KEY, activity TEXT NOT NULL, start TEXT NOT NULL, end TEXT NOT NULL, checked INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS records_day ON records(day);
             DROP TABLE IF EXISTS stats;
             DROP TABLE IF EXISTS coverage;
             DROP TABLE IF EXISTS meta;",
        )?;
        let old: Option<String> = tx
            .query_row("SELECT scope FROM identity LIMIT 1", [], |r| r.get(0))
            .optional()?;
        if old.as_deref() != Some(scope) {
            if !old.as_ref().is_some_and(|old| legacy.contains(old)) {
                tx.execute_batch("DELETE FROM records; DELETE FROM synced;")?;
            }
            tx.execute("DELETE FROM identity", [])?;
            tx.execute("INSERT INTO identity VALUES(?1)", [scope])?;
        }
        tx.commit()?;
        Ok(Self {
            db,
            pages_left: HISTORY_PAGES_PER_REFRESH,
        })
    }

    pub(super) fn refund_empty_page(&mut self) {
        self.pages_left += 1;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adopts_old_platform_cache_only_for_the_same_instance_and_login() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("usage.db");
        let url = "https://relay.example";
        let user = "admin@example.com";
        let legacy = sha256_hex(format!("{url}\0{user}\0{}", Sub2ApiUpstream::Claude).as_bytes());
        let cache = UsageCache::open(&path, &legacy).unwrap();
        cache
            .db
            .execute("INSERT INTO records VALUES(7,1,'2026-11-25','{}')", [])
            .unwrap();
        drop(cache);
        for (login, expected) in [(user, 1), (user, 1), ("another@example.com", 0)] {
            let cache = UsageCache::for_connection(&path, url, login).unwrap();
            let count: i64 = cache
                .db
                .query_row("SELECT count(*) FROM records", [], |r| r.get(0))
                .unwrap();
            assert_eq!(count, expected);
        }
    }
}
