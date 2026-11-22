//! HistoryStore 语义：事务累计、计数器重置、持久化游标与迁移。

use chrono::{DateTime, Utc};
use trafficview_daemon::history::HistoryStore;

fn at(value: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(value)
        .unwrap()
        .with_timezone(&Utc)
}

fn memory() -> HistoryStore {
    HistoryStore::open_memory().unwrap()
}

fn open_file(path: &std::path::Path) -> HistoryStore {
    HistoryStore::open(path, "UTC", 90).unwrap()
}

#[test]
fn minute_and_day_totals_are_updated_in_one_transaction() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .add_delta(id, at("2026-11-20T10:00:20Z"), 1024, 2048)
        .unwrap();
    store
        .add_delta(id, at("2026-11-20T10:00:50Z"), 1024, 0)
        .unwrap();
    assert_eq!(store.minute(id, "2026-11-20T10:00Z").unwrap(), (2048, 2048));
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (2048, 2048));
}

#[test]
fn counter_reset_rebases_without_negative_or_duplicate_totals() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .observe(id, at("2026-11-20T10:00:00Z"), 1000, 2000)
        .unwrap();
    store
        .observe(id, at("2026-11-20T10:01:00Z"), 1500, 2600)
        .unwrap();
    store
        .observe(id, at("2026-11-20T10:02:00Z"), 10, 20)
        .unwrap();
    store
        .observe(id, at("2026-11-20T10:03:00Z"), 110, 220)
        .unwrap();
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (600, 800));
}

#[test]
fn equal_counters_add_nothing() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .observe(id, at("2026-11-20T10:00:00Z"), 500, 600)
        .unwrap();
    store
        .observe(id, at("2026-11-20T10:01:00Z"), 500, 600)
        .unwrap();
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (0, 0));
}

#[test]
fn repeated_observe_in_the_same_minute_does_not_duplicate_totals() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .observe(id, at("2026-11-20T10:00:10Z"), 100, 200)
        .unwrap();
    for second in [20, 30, 40, 50] {
        store
            .observe(id, at(&format!("2026-11-20T10:00:{second}Z")), 100, 200)
            .unwrap();
    }
    assert_eq!(store.minute(id, "2026-11-20T10:00Z").unwrap(), (0, 0));
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (0, 0));
}

#[test]
fn persistent_cursor_continues_after_reopen() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    {
        let mut store = open_file(&path);
        let id = store.upsert_interface("mac:aa", "eth0").unwrap();
        store
            .observe(id, at("2026-11-20T10:00:00Z"), 1000, 2000)
            .unwrap();
    }
    let mut store = open_file(&path);
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .observe(id, at("2026-11-20T10:01:00Z"), 1500, 2600)
        .unwrap();
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (500, 600));
}

#[test]
fn reopening_does_not_rerun_migration_or_reset_data() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    for _ in 0..3 {
        let mut store = open_file(&path);
        let id = store.upsert_interface("mac:aa", "eth0").unwrap();
        store
            .add_delta(id, at("2026-11-20T10:00:00Z"), 10, 20)
            .unwrap();
    }
    let mut store = open_file(&path);
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (30, 60));
}

#[test]
fn invalid_schema_version_returns_a_diagnostic_error() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("history.db");
    {
        let connection = rusqlite::Connection::open(&path).unwrap();
        connection.pragma_update(None, "user_version", 99).unwrap();
    }
    let error = HistoryStore::open(&path, "UTC", 90).unwrap_err();
    assert!(
        error.to_string().contains("schema version 99"),
        "unexpected error: {error}"
    );
}

#[test]
fn counters_above_i64_max_are_rejected_instead_of_wrapping() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    let error = store
        .observe(id, at("2026-11-20T10:00:00Z"), u64::MAX, 0)
        .unwrap_err();
    assert!(error.to_string().contains("i64::MAX"), "{error}");
    let error = store
        .add_delta(id, at("2026-11-20T10:00:00Z"), 0, u64::MAX)
        .unwrap_err();
    assert!(error.to_string().contains("i64::MAX"), "{error}");
}

#[test]
fn renamed_interface_keeps_the_same_history_row() {
    let mut store = memory();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .add_delta(id, at("2026-11-20T10:00:00Z"), 10, 20)
        .unwrap();
    let renamed = store.upsert_interface("mac:aa", "enp0s3").unwrap();
    assert_eq!(id, renamed);
    assert_eq!(store.day(id, "2026-11-20").unwrap(), (10, 20));
    assert_eq!(
        store.interfaces().unwrap(),
        vec![("mac:aa".to_owned(), "enp0s3".to_owned())]
    );
}

#[test]
fn summaries_cover_day_and_month_and_reject_unknown_scope() {
    let mut store = HistoryStore::open_memory_with_tz(8 * 60).unwrap();
    let eth0 = store.upsert_interface("mac:aa", "eth0").unwrap();
    let wlan = store.upsert_interface("mac:bb", "wlan0").unwrap();
    store
        .add_delta(eth0, at("2026-10-31T16:30:00Z"), 100, 200)
        .unwrap(); // 2026-11-01 00:30 (UTC+8)
    store
        .add_delta(wlan, at("2026-11-20T10:00:00Z"), 7, 8)
        .unwrap();
    store
        .add_delta(eth0, at("2026-11-20T10:00:00Z"), 1, 2)
        .unwrap();

    let day = store
        .summary("day", None, at("2026-11-20T12:00:00Z"))
        .unwrap();
    assert_eq!((day.rx, day.tx), (8, 10));
    assert_eq!(day.start, "2026-11-20");
    assert_eq!(day.end, "2026-11-20");
    assert_eq!(day.interfaces.len(), 2);

    let month = store
        .summary("month", None, at("2026-11-20T12:00:00Z"))
        .unwrap();
    assert_eq!((month.rx, month.tx), (108, 210));
    assert_eq!(month.start, "2026-11-01");
    assert_eq!(month.end, "2026-11-30");

    let only = store
        .summary("month", Some("mac:bb"), at("2026-11-20T12:00:00Z"))
        .unwrap();
    assert_eq!((only.rx, only.tx), (7, 8));

    let error = store
        .summary("year", None, at("2026-11-20T12:00:00Z"))
        .unwrap_err();
    assert!(error.to_string().contains("invalid history scope"));
}

#[test]
fn retention_prunes_minutes_but_keeps_daily_totals() {
    let mut store = HistoryStore::open_memory_with_tz(0).unwrap();
    let id = store.upsert_interface("mac:aa", "eth0").unwrap();
    store
        .add_delta(id, at("2026-01-01T10:00:00Z"), 1, 2)
        .unwrap();
    // 首次采样的清理以自身时间为基准，旧明细仍然存在。
    assert_eq!(store.minute(id, "2026-01-01T10:00Z").unwrap(), (1, 2));
    // 新的一天触发保留策略：分钟明细被清理，日汇总保留。
    store
        .add_delta(id, at("2026-11-20T10:00:00Z"), 3, 4)
        .unwrap();
    assert_eq!(store.minute(id, "2026-01-01T10:00Z").unwrap(), (0, 0));
    assert_eq!(store.day(id, "2026-01-01").unwrap(), (1, 2));
    assert!(store.prune(at("2026-11-20T10:00:00Z")).unwrap() == 0);
}
