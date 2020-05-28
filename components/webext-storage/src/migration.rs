/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::error::*;
use rusqlite::{Connection, OpenFlags, Transaction, NO_PARAMS};
use serde_json::{Map, Value};
use sql_support::ConnExt;
use std::convert::TryInto;
use std::path::Path;

// Simple migration from the "old" kinto-with-sqlite-backing implementation
// to ours.
// Could almost be trivially done in JS using the regular public API if not
// for:
// * We don't want to enforce the same quotas when migrating.
// * We'd rather do the entire migration in a single transaction for perf
//   reasons.

// The sqlite database we migrate from has a very simple structure:
// * table collection_data with columns collection_name, record_id and record
// * `collection_name` is a string of form "default/{extension_id}"
// * `record_id` is `key-{key}`
// * `record` is a string with json, of form: {
//     id: {the record id repeated},
//     key: {the key},
//     data: {the actual data},
//     _status: {sync status},
//     last_modified: {timestamp},
//  }
// So the info we need is stored somewhat redundantly.
// Further:
// * There's a special collection_name "default/storage-sync-crypto" that
//   we don't want to migrate. Its record_id is 'keys' and its json has no
//   `data`

// Note we don't enforce a quota - we migrate everything - even if this means
// it's too big for the server to store. This is a policy decision - it's better
// to not lose data than to try and work out what data can be disposed of, as
// the addon has the ability to determine this.

// Our error strategy is "ignore read errors, propagate write errors" under the
// assumption that the former tends to mean a damaged DB or file-system and is
// unlikely to work if we try later (eg, replacing the disk isn't likely to
// uncorrupt the DB), where the latter is likely to be disk-space or file-system
// error, but retry might work (eg, replacing the disk then trying again might
// make the writes work)

// The struct we read from the DB.
struct LegacyRow {
    col_name: String, // collection_name column
    record: String,   // record column
}

impl LegacyRow {
    // Parse the 2 columns from the DB into the data we need to insert into
    // our target database.
    fn parse(&self) -> Option<Parsed<'_>> {
        if self.col_name.len() < 8 {
            log::trace!("collection_name of '{}' is too short", self.col_name);
            return None;
        }
        if &self.col_name[..8] != "default/" {
            log::trace!("collection_name of '{}' isn't ours", self.col_name);
            return None;
        }
        let ext_id = &self.col_name[8..];
        let mut record_map = match serde_json::from_str(&self.record) {
            Ok(Value::Object(m)) => m,
            Ok(o) => {
                log::info!("skipping non-json-object 'record' column");
                log::trace!("record value is json, but not an object: {}", o);
                return None;
            }
            Err(e) => {
                log::info!("skipping non-json 'record' column");
                log::trace!("record value isn't json: {}", e);
                return None;
            }
        };

        let key = match record_map.remove("key") {
            Some(Value::String(s)) if !s.is_empty() => s,
            Some(o) => {
                log::trace!("key is json but not a string: {}", o);
                return None;
            }
            _ => {
                log::trace!("key doesn't exist in the map");
                return None;
            }
        };
        let data = match record_map.remove("data") {
            Some(d) => d,
            _ => {
                log::trace!("data doesn't exist in the map");
                return None;
            }
        };
        Some(Parsed { ext_id, key, data })
    }
}

// The info we parse from the raw DB strings.
struct Parsed<'a> {
    ext_id: &'a str,
    key: String,
    data: serde_json::Value,
}

pub fn migrate(tx: &Transaction<'_>, filename: &Path) -> Result<MigrationInfo> {
    // We do the grouping manually, collecting string values as we go.
    let mut last_ext_id = "".to_string();
    let mut curr_values: Vec<(String, serde_json::Value)> = Vec::new();
    let mut curr_quota_bytes: usize = 0;
    let (rows, mut mi) = read_rows(filename);
    for row in rows {
        log::trace!("processing '{}' - '{}'", row.col_name, row.record);
        let parsed = match row.parse() {
            Some(p) => p,
            None => {
                mi.non_fatal_errors_seen += 1;
                continue;
            }
        };
        // Do our "grouping"
        if parsed.ext_id != last_ext_id {
            if last_ext_id != "" && !curr_values.is_empty() {
                // a different extension id - write what we have to the DB.
                let entries = do_insert(tx, &last_ext_id, curr_values)?;
                mi.extensions_successful += 1;
                mi.entries_successful += entries;
            }
            last_ext_id = parsed.ext_id.to_string();
            curr_values = Vec::new();
            curr_quota_bytes = 0;
        }
        // no 'else' here - must also enter this block on ext_id change.
        if parsed.ext_id == last_ext_id {
            let quota_bytes = crate::api::get_quota_size_of(&parsed.key, &parsed.data);
            curr_quota_bytes += quota_bytes;
            if quota_bytes > crate::api::SYNC_QUOTA_BYTES_PER_ITEM
                || curr_quota_bytes > crate::api::SYNC_QUOTA_BYTES
            {
                mi.overflowed_quotas = true;
            }
            curr_values.push((parsed.key.to_string(), parsed.data));
            log::trace!(
                "extension {} now has {} keys",
                parsed.ext_id,
                curr_values.len()
            );
        }
    }
    // and the last one
    if last_ext_id != "" && !curr_values.is_empty() {
        // a different extension id - write what we have to the DB.
        let entries = do_insert(tx, &last_ext_id, curr_values)?;
        mi.extensions_successful += 1;
        mi.entries_successful += entries;
    }
    log::info!("migrated {} extensions: {:?}", mi.extensions_successful, mi);
    Ok(mi)
}

fn read_rows(filename: &Path) -> (Vec<LegacyRow>, MigrationInfo) {
    let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX | OpenFlags::SQLITE_OPEN_READ_ONLY;
    let src_conn = match Connection::open_with_flags(&filename, flags) {
        Ok(conn) => conn,
        Err(e) => {
            log::warn!("Failed to open the source DB: {}", e);
            return (
                Vec::new(),
                MigrationInfo {
                    non_fatal_errors_seen: 1,
                    open_failure: true,
                    ..Default::default()
                },
            );
        }
    };
    // Failure to prepare the statement probably just means the source DB is
    // damaged.
    let mut stmt = match src_conn.prepare(
        "SELECT collection_name, record FROM collection_data
         WHERE collection_name != 'default/storage-sync-crypto'
         ORDER BY collection_name",
    ) {
        Ok(stmt) => stmt,
        Err(e) => {
            log::warn!("Failed to prepare the statement: {}", e);
            return (
                Vec::new(),
                MigrationInfo {
                    non_fatal_errors_seen: 1,
                    // I believe the only case where this would happen is if (we
                    // have a bug or) the source database has a corrupted
                    // schema. It doesn't really seem worth distinguishing that
                    // from "complete read failure" cases, so here we are.
                    open_failure: true,
                    ..Default::default()
                },
            );
        }
    };
    let rows = match stmt.query_and_then(NO_PARAMS, |row| -> Result<LegacyRow> {
        Ok(LegacyRow {
            col_name: row.get(0)?,
            record: row.get(1)?,
        })
    }) {
        Ok(r) => r,
        Err(e) => {
            log::warn!("Failed to read any rows from the source DB: {}", e);
            return (
                Vec::new(),
                MigrationInfo {
                    non_fatal_errors_seen: 1,
                    open_failure: true,
                    ..Default::default()
                },
            );
        }
    };
    let rows: Vec<LegacyRow> = rows.filter_map(Result::ok).collect();

    let entries = src_conn.query_row::<i64, _, _>(
        "SELECT COUNT(*) FROM collection_data
            WHERE collection_name != 'default/storage-sync-crypto'",
        NO_PARAMS,
        |r| r.get(0),
    );
    let entries_is_err = entries.is_err();
    let entries: usize = entries.unwrap_or_default().try_into().unwrap_or_default();

    let extensions = src_conn.query_row::<i64, _, _>(
        "SELECT COUNT(DISTINCT collection_name) FROM collection_data
            WHERE collection_name != 'default/storage-sync-crypto'",
        NO_PARAMS,
        |r| r.get(0),
    );

    let extensions_is_err = extensions.is_err();
    let extensions: usize = extensions
        .unwrap_or_default()
        .try_into()
        .unwrap_or_default();

    // If rows.len() > entries it means we saw an error in the COUNT call (or
    // something is very weird), but this would be caught by `entries_is_err`.
    let entries_failed = entries.saturating_sub(rows.len());

    let non_fatal_errors_seen =
        entries_failed + (extensions_is_err as usize) + (entries_is_err as usize);

    let mi = MigrationInfo {
        entries,
        extensions,
        non_fatal_errors_seen,
        // Populated later.
        extensions_successful: 0,
        entries_successful: 0,
        overflowed_quotas: false,
        open_failure: false,
    };

    (rows, mi)
}

/// Insert the extension and values. If there are multiple values with the same
/// key (which shouldn't be possible but who knows, database corruption causes
/// strange things), chooses an arbitrary one. Returns the number of entries
/// inserted, which could be different from `vals.len()` if multiple entries in
/// `vals` have the same key.
fn do_insert(tx: &Transaction<'_>, ext_id: &str, vals: Vec<(String, Value)>) -> Result<usize> {
    let mut map = Map::with_capacity(vals.len());
    for (key, val) in vals {
        map.insert(key, val);
    }
    let num_entries = map.len();
    tx.execute_named_cached(
        "INSERT OR REPLACE INTO storage_sync_data(ext_id, data, sync_change_counter)
         VALUES (:ext_id, :data, 1)",
        rusqlite::named_params! {
            ":ext_id": &ext_id,
            ":data": &Value::Object(map),
        },
    )?;
    Ok(num_entries)
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct MigrationInfo {
    /// The number of entries (rows in the original table) we attempted to
    /// migrate. Zero if there was some error in computing this number.
    pub entries: usize,
    /// The number of records we successfully migrated (equal to `entries` for
    /// entirely successful migrations).
    pub entries_successful: usize,
    /// The number of extensions (distinct extension ids) in the original
    /// table.
    pub extensions: usize,
    /// The number of extensions we successfully migrated
    pub extensions_successful: usize,
    /// A count of recoverable non-fatal errors seen. Generally this will cause
    /// the failed count to be nonzero, but there are other cases that can cause
    /// error (failing to compute # of entries or extensions).
    pub non_fatal_errors_seen: usize,
    /// True if we hit at least one quota error.
    pub overflowed_quotas: bool,
    /// True iff we failed to open the source DB at all.
    pub open_failure: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api;
    use crate::db::{test::new_mem_db, StorageDb};
    use serde_json::json;
    use tempfile::tempdir;

    // Create a test database, populate it via the callback, migrate it, and
    // return a connection to the new, migrated DB for further checking.
    fn do_migrate<F>(expect_mi: MigrationInfo, f: F) -> StorageDb
    where
        F: FnOnce(&Connection),
    {
        let tmpdir = tempdir().unwrap();
        let path = tmpdir.path().join("source.db");
        let flags = OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_READ_WRITE;
        let mut conn = Connection::open_with_flags(path, flags).expect("open should work");
        let tx = conn.transaction().expect("should be able to get a tx");
        tx.execute_batch(
            "CREATE TABLE collection_data (
                collection_name TEXT,
                record_id TEXT,
                record TEXT
            );",
        )
        .expect("create should work");
        f(&tx);
        tx.commit().expect("should commit");
        conn.close().expect("close should work");

        // now migrate
        let mut db = new_mem_db();
        let tx = db.transaction().expect("tx should work");

        let mi = migrate(&tx, &tmpdir.path().join("source.db")).expect("migrate should work");
        tx.commit().expect("should work");
        assert_eq!(mi, expect_mi);
        db
    }

    fn assert_has(c: &Connection, ext_id: &str, expect: Value) {
        assert_eq!(
            api::get(c, ext_id, json!(null)).expect("should get"),
            expect
        );
    }

    #[allow(clippy::unreadable_literal)]
    #[test]
    fn test_happy_paths() {
        // some real data.
        let conn = do_migrate(
            MigrationInfo {
                entries: 5,
                entries_successful: 5,
                extensions: 2,
                extensions_successful: 2,
                non_fatal_errors_seen: 0,
                overflowed_quotas: false,
                open_failure: false,
            },
            |c| {
                c.execute_batch(
                r#"INSERT INTO collection_data(collection_name, record)
                    VALUES
                    ('default/{e7fefcf3-b39c-4f17-5215-ebfe120a7031}', '{"id":"key-userWelcomed","key":"userWelcomed","data":1570659224457,"_status":"synced","last_modified":1579755940527}'),
                    ('default/{e7fefcf3-b39c-4f17-5215-ebfe120a7031}', '{"id":"key-isWho","key":"isWho","data":"4ec8109f","_status":"synced","last_modified":1579755940497}'),
                    ('default/storage-sync-crypto', '{"id":"keys","keys":{"default":["rQ=","lR="],"collections":{"extension@redux.devtools":["Bd=","ju="]}}}'),
                    ('default/https-everywhere@eff.org', '{"id":"key-userRules","key":"userRules","data":[],"_status":"synced","last_modified":1570079920045}'),
                    ('default/https-everywhere@eff.org', '{"id":"key-ruleActiveStates","key":"ruleActiveStates","data":{},"_status":"synced","last_modified":1570079919993}'),
                    ('default/https-everywhere@eff.org', '{"id":"key-migration_5F_version","key":"migration_version","data":2,"_status":"synced","last_modified":1570079919966}')
                    "#,
            ).expect("should popuplate")
            },
        );

        assert_has(
            &conn,
            "{e7fefcf3-b39c-4f17-5215-ebfe120a7031}",
            json!({"userWelcomed": 1570659224457u64, "isWho": "4ec8109f"}),
        );
        assert_has(
            &conn,
            "https-everywhere@eff.org",
            json!({"userRules": [], "ruleActiveStates": {}, "migration_version": 2}),
        );
    }

    #[test]
    fn test_sad_paths() {
        do_migrate(
            MigrationInfo {
                entries: 10,
                entries_successful: 0,
                extensions: 6,
                extensions_successful: 0,
                non_fatal_errors_seen: 9,
                overflowed_quotas: false,
                open_failure: false,
            },
            |c| {
                c.execute_batch(
                    r#"INSERT INTO collection_data(collection_name, record)
                    VALUES
                    ('default/test', '{"key":2,"data":1}'), -- key not a string
                    ('default/test', '{"key":"","data":1}'), -- key empty string
                    ('default/test', '{"xey":"k","data":1}'), -- key missing
                    ('default/test', '{"key":"k","xata":1}'), -- data missing
                    ('default/test', '{"key":"k","data":1'), -- invalid json
                    ('xx/test', '{"key":"k","data":1}'), -- bad key format
                    ('default', '{"key":"k","data":1}'), -- bad key format 2
                    ('default/', '{"key":"k","data":1}'), -- bad key format 3
                    ('defaultx/test', '{"key":"k","data":1}'), -- bad key format 4
                    ('', '') -- empty strings
                    "#,
                )
                .expect("should populate");
            },
        );
    }
}
