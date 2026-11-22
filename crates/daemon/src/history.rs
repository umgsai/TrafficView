//! SQLite 历史：分钟明细、日汇总与每卡原始计数器游标。
//!
//! 所有写入都在一个短事务里完成“分钟行 + 日行 + 游标”，重启后从持久化游标继续。
//! 分钟行按 UTC 向下取整到 60 秒；日键按配置时区生成（默认系统本地时区）。

use std::{
    fmt, fs, io,
    path::{Path, PathBuf},
    time::Duration,
};

use chrono::{DateTime, Datelike, FixedOffset, Local, NaiveDate, NaiveDateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use trafficview_core::{config::parse_offset_minutes, InterfaceTraffic, TrafficSummary};

/// 当前 schema 版本；遇到更高版本时拒绝启动而不尝试降级。
pub const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE iface(
  id INTEGER PRIMARY KEY,
  stable TEXT UNIQUE NOT NULL,
  name TEXT NOT NULL,
  last_rx INTEGER,
  last_tx INTEGER
);
CREATE TABLE traffic_min(
  iface_id INTEGER NOT NULL,
  ts INTEGER NOT NULL,
  rx INTEGER NOT NULL DEFAULT 0,
  tx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(iface_id, ts)
) WITHOUT ROWID;
CREATE TABLE traffic_day(
  iface_id INTEGER NOT NULL,
  ymd TEXT NOT NULL,
  rx INTEGER NOT NULL DEFAULT 0,
  tx INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY(iface_id, ymd)
) WITHOUT ROWID;
PRAGMA user_version=1;
";

#[derive(Debug)]
pub enum HistoryError {
    Sql(rusqlite::Error),
    Io(io::Error),
    /// 数据库版本不是本程序能安全处理的版本。
    Schema(String),
    /// 计数器超出 SQLite 有符号 64 位范围。
    Overflow(String),
}

impl fmt::Display for HistoryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sql(error) => write!(f, "history database error: {error}"),
            Self::Io(error) => write!(f, "history I/O error: {error}"),
            Self::Schema(message) => write!(f, "unsupported history database: {message}"),
            Self::Overflow(message) => write!(f, "history counter overflow: {message}"),
        }
    }
}

impl std::error::Error for HistoryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Sql(error) => Some(error),
            Self::Io(error) => Some(error),
            Self::Schema(_) | Self::Overflow(_) => None,
        }
    }
}

impl From<rusqlite::Error> for HistoryError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Sql(error)
    }
}

impl From<io::Error> for HistoryError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// 单进程内的历史存储；短事务，供采样线程与查询共享（外层用 Mutex 串行化）。
pub struct HistoryStore {
    conn: Connection,
    tz: FixedOffset,
    retention_days: u32,
    last_cleanup_ymd: Option<String>,
}

impl fmt::Debug for HistoryStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("HistoryStore")
            .field("tz", &self.tz)
            .field("retention_days", &self.retention_days)
            .finish_non_exhaustive()
    }
}

impl HistoryStore {
    /// 测试用内存库，固定 UTC 以便断言日键。
    pub fn open_memory() -> Result<Self, HistoryError> {
        Self::open_memory_with_tz(0)
    }

    /// 内存库，显式时区偏移（分钟）。
    pub fn open_memory_with_tz(offset_minutes: i32) -> Result<Self, HistoryError> {
        let conn = Connection::open_in_memory()?;
        Self::finish(conn, offset_minutes, 90)
    }

    /// 磁盘库；`timezone` 为空表示系统本地时区。
    pub fn open(
        path: impl AsRef<Path>,
        timezone: &str,
        retention_days: u32,
    ) -> Result<Self, HistoryError> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        Self::finish(conn, resolve_tz_offset(timezone), retention_days)
    }

    /// `$XDG_DATA_HOME/trafficview/history.db`，未设置时 `$HOME/.local/share/...`。
    pub fn default_path() -> Option<PathBuf> {
        if let Some(dir) = std::env::var_os("XDG_DATA_HOME").filter(|value| !value.is_empty()) {
            return Some(PathBuf::from(dir).join("trafficview").join("history.db"));
        }
        std::env::var_os("HOME")
            .filter(|value| !value.is_empty())
            .map(|home| {
                PathBuf::from(home)
                    .join(".local")
                    .join("share")
                    .join("trafficview")
                    .join("history.db")
            })
    }

    fn finish(
        conn: Connection,
        offset_minutes: i32,
        retention_days: u32,
    ) -> Result<Self, HistoryError> {
        conn.busy_timeout(Duration::from_millis(5000))?;
        // WAL 对内存库返回 memory，不视为错误。
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))?;
        // WAL + NORMAL：提交不再每次 fsync，掉电最多丢失最近若干采样，
        // 不会损坏数据库。历史统计不值得为此每秒付一次磁盘同步。
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&conn)?;
        Ok(Self {
            conn,
            tz: FixedOffset::east_opt(
                offset_minutes
                    .checked_mul(60)
                    .ok_or_else(|| HistoryError::Schema("invalid timezone offset".into()))?,
            )
            .ok_or_else(|| HistoryError::Schema("invalid timezone offset".into()))?,
            retention_days: retention_days.max(1),
            last_cleanup_ymd: None,
        })
    }

    /// 稳定键优先为 MAC；同 MAC 改名时更新展示名并保留同一历史行。
    pub fn upsert_interface(&mut self, stable: &str, name: &str) -> Result<i64, HistoryError> {
        let id = self.conn.query_row(
            "INSERT INTO iface(stable, name) VALUES(?1, ?2)
             ON CONFLICT(stable) DO UPDATE SET name=excluded.name
             RETURNING id",
            params![stable, name],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// 记录一次原始计数器采样；任一计数器倒退时只重置游标。
    pub fn observe(
        &mut self,
        id: i64,
        at: DateTime<Utc>,
        rx_total: u64,
        tx_total: u64,
    ) -> Result<(), HistoryError> {
        let rx = to_i64(rx_total, "rx")?;
        let tx = to_i64(tx_total, "tx")?;
        let transaction = self.conn.transaction()?;
        let cursor: Option<(Option<i64>, Option<i64>)> = transaction
            .query_row(
                "SELECT last_rx, last_tx FROM iface WHERE id=?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let (last_rx, last_tx) =
            cursor.ok_or_else(|| HistoryError::Schema(format!("interface id {id} not found")))?;
        let (delta_rx, delta_tx) = match (last_rx, last_tx) {
            (Some(previous_rx), Some(previous_tx)) if rx >= previous_rx && tx >= previous_tx => {
                (rx - previous_rx, tx - previous_tx)
            }
            _ => (0, 0),
        };
        if delta_rx != 0 || delta_tx != 0 {
            add_rows(&transaction, id, at, self.tz, delta_rx, delta_tx)?;
        }
        transaction.execute(
            "UPDATE iface SET last_rx=?2, last_tx=?3 WHERE id=?1",
            params![id, rx, tx],
        )?;
        transaction.commit()?;
        self.cleanup_if_new_day(at)?;
        Ok(())
    }

    /// 直接累加一段非负增量（`observe` 复用同一个行写入逻辑）。
    pub fn add_delta(
        &mut self,
        id: i64,
        at: DateTime<Utc>,
        rx_delta: u64,
        tx_delta: u64,
    ) -> Result<(), HistoryError> {
        let rx = to_i64(rx_delta, "rx delta")?;
        let tx = to_i64(tx_delta, "tx delta")?;
        let transaction = self.conn.transaction()?;
        add_rows(&transaction, id, at, self.tz, rx, tx)?;
        transaction.commit()?;
        self.cleanup_if_new_day(at)?;
        Ok(())
    }

    pub fn minute(&self, id: i64, key: &str) -> Result<(u64, u64), HistoryError> {
        let ts = parse_minute_key(key)?;
        self.read_pair(
            "SELECT rx, tx FROM traffic_min WHERE iface_id=?1 AND ts=?2",
            params![id, ts],
        )
    }

    pub fn day(&self, id: i64, ymd: &str) -> Result<(u64, u64), HistoryError> {
        self.read_pair(
            "SELECT rx, tx FROM traffic_day WHERE iface_id=?1 AND ymd=?2",
            params![id, ymd],
        )
    }

    fn read_pair(
        &self,
        sql: &str,
        params: impl rusqlite::Params,
    ) -> Result<(u64, u64), HistoryError> {
        let pair: Option<(i64, i64)> = self
            .conn
            .query_row(sql, params, |row| Ok((row.get(0)?, row.get(1)?)))
            .optional()?;
        let (rx, tx) = pair.unwrap_or((0, 0));
        Ok((from_i64(rx, "rx")?, from_i64(tx, "tx")?))
    }

    /// 按日/月汇总；`stable=None` 表示所有网卡求和。
    pub fn summary(
        &self,
        scope: &str,
        stable: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<TrafficSummary, HistoryError> {
        let local = now.with_timezone(&self.tz);
        let (start_key, end_key) = match scope {
            "day" => {
                let date = local.date_naive();
                let key = date.format("%Y-%m-%d").to_string();
                (key.clone(), key)
            }
            "month" => {
                let first = NaiveDate::from_ymd_opt(local.year(), local.month(), 1)
                    .ok_or_else(|| HistoryError::Schema("invalid month".into()))?;
                let last = last_day_of_month(first);
                (
                    first.format("%Y-%m-%d").to_string(),
                    last.format("%Y-%m-%d").to_string(),
                )
            }
            other => {
                return Err(HistoryError::Schema(format!(
                    "invalid history scope '{other}'; use day or month"
                )))
            }
        };
        let (sql, bind_stable): (&str, Option<&str>) = match stable {
            Some(stable) => (
                "SELECT i.stable, i.name, COALESCE(SUM(d.rx),0), COALESCE(SUM(d.tx),0)
                 FROM traffic_day d JOIN iface i ON i.id=d.iface_id
                 WHERE d.ymd BETWEEN ?1 AND ?2 AND i.stable=?3
                 GROUP BY i.id ORDER BY i.name",
                Some(stable),
            ),
            None => (
                "SELECT i.stable, i.name, COALESCE(SUM(d.rx),0), COALESCE(SUM(d.tx),0)
                 FROM traffic_day d JOIN iface i ON i.id=d.iface_id
                 WHERE d.ymd BETWEEN ?1 AND ?2
                 GROUP BY i.id ORDER BY i.name",
                None,
            ),
        };
        let mut statement = self.conn.prepare(sql)?;
        let mut map_row = |row: &rusqlite::Row<'_>| -> rusqlite::Result<InterfaceTraffic> {
            Ok(InterfaceTraffic {
                stable: row.get(0)?,
                name: row.get(1)?,
                rx: row.get::<_, i64>(2)? as u64,
                tx: row.get::<_, i64>(3)? as u64,
            })
        };
        let interfaces: Vec<InterfaceTraffic> = match bind_stable {
            Some(stable) => statement
                .query_map(params![start_key, end_key, stable], &mut map_row)?
                .collect::<Result<_, _>>()?,
            None => statement
                .query_map(params![start_key, end_key], &mut map_row)?
                .collect::<Result<_, _>>()?,
        };
        let rx = interfaces
            .iter()
            .try_fold(0u64, |acc, iface| acc.checked_add(iface.rx))
            .ok_or_else(|| HistoryError::Overflow("summary rx total".into()))?;
        let tx = interfaces
            .iter()
            .try_fold(0u64, |acc, iface| acc.checked_add(iface.tx))
            .ok_or_else(|| HistoryError::Overflow("summary tx total".into()))?;
        Ok(TrafficSummary {
            scope: scope.to_owned(),
            start: start_key,
            end: end_key,
            rx,
            tx,
            interfaces,
        })
    }

    /// 已知网卡的 stable/name 列表，供 CLI 与 D-Bus 列举。
    pub fn interfaces(&self) -> Result<Vec<(String, String)>, HistoryError> {
        let mut statement = self
            .conn
            .prepare("SELECT stable, name FROM iface ORDER BY name")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// 删除 `retention_days` 之前的分钟明细；日汇总保留。
    pub fn prune(&mut self, now: DateTime<Utc>) -> Result<usize, HistoryError> {
        let cutoff = now.timestamp() - i64::from(self.retention_days) * 86_400;
        let removed = self
            .conn
            .execute("DELETE FROM traffic_min WHERE ts < ?1", params![cutoff])?;
        Ok(removed)
    }

    fn cleanup_if_new_day(&mut self, at: DateTime<Utc>) -> Result<(), HistoryError> {
        let today = at.with_timezone(&self.tz).format("%Y-%m-%d").to_string();
        if self.last_cleanup_ymd.as_deref() == Some(today.as_str()) {
            return Ok(());
        }
        self.prune(at)?;
        self.last_cleanup_ymd = Some(today);
        Ok(())
    }

    fn ymd(&self, at: DateTime<Utc>) -> String {
        at.with_timezone(&self.tz).format("%Y-%m-%d").to_string()
    }

    #[doc(hidden)]
    pub fn day_key(&self, at: DateTime<Utc>) -> String {
        self.ymd(at)
    }
}

fn migrate(conn: &Connection) -> Result<(), HistoryError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    match version {
        0 => {
            conn.execute_batch(SCHEMA)?;
            Ok(())
        }
        SCHEMA_VERSION => Ok(()),
        other => Err(HistoryError::Schema(format!(
            "schema version {other} is newer than supported version {SCHEMA_VERSION}"
        ))),
    }
}

fn add_rows(
    transaction: &Transaction<'_>,
    id: i64,
    at: DateTime<Utc>,
    tz: FixedOffset,
    rx: i64,
    tx: i64,
) -> Result<(), HistoryError> {
    let minute = at.timestamp().div_euclid(60) * 60;
    let ymd = at.with_timezone(&tz).format("%Y-%m-%d").to_string();
    transaction.execute(
        "INSERT INTO traffic_min(iface_id, ts, rx, tx) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(iface_id, ts) DO UPDATE SET rx=rx+excluded.rx, tx=tx+excluded.tx",
        params![id, minute, rx, tx],
    )?;
    transaction.execute(
        "INSERT INTO traffic_day(iface_id, ymd, rx, tx) VALUES(?1, ?2, ?3, ?4)
         ON CONFLICT(iface_id, ymd) DO UPDATE SET rx=rx+excluded.rx, tx=tx+excluded.tx",
        params![id, ymd, rx, tx],
    )?;
    Ok(())
}

fn parse_minute_key(key: &str) -> Result<i64, HistoryError> {
    NaiveDateTime::parse_from_str(key, "%Y-%m-%dT%H:%MZ")
        .map(|naive| naive.and_utc().timestamp())
        .map_err(|error| HistoryError::Schema(format!("invalid minute key '{key}': {error}")))
}

fn resolve_tz_offset(spec: &str) -> i32 {
    if spec.is_empty() {
        Local::now().offset().local_minus_utc() / 60
    } else if spec == "UTC" {
        0
    } else {
        parse_offset_minutes(spec).unwrap_or(0)
    }
}

fn last_day_of_month(first: NaiveDate) -> NaiveDate {
    let (year, month) = if first.month() == 12 {
        (first.year() + 1, 1)
    } else {
        (first.year(), first.month() + 1)
    };
    NaiveDate::from_ymd_opt(year, month, 1)
        .and_then(|next| next.pred_opt())
        .unwrap_or(first)
}

fn to_i64(value: u64, label: &str) -> Result<i64, HistoryError> {
    i64::try_from(value)
        .map_err(|_| HistoryError::Overflow(format!("{label} value {value} exceeds i64::MAX")))
}

fn from_i64(value: i64, label: &str) -> Result<u64, HistoryError> {
    u64::try_from(value).map_err(|_| {
        HistoryError::Overflow(format!(
            "stored {label} value {value} is negative or too large"
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(value: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(value)
            .unwrap()
            .with_timezone(&Utc)
    }

    #[test]
    fn invalid_schema_version_is_reported() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        let error = migrate(&conn).unwrap_err();
        assert!(error.to_string().contains("newer than supported"));
    }

    #[test]
    fn migration_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    #[test]
    fn day_key_follows_the_explicit_timezone() {
        let store = HistoryStore::open_memory_with_tz(8 * 60).unwrap();
        // UTC 16:30 = 次日 00:30 (UTC+8)。
        assert_eq!(store.day_key(at("2026-11-20T16:30:00Z")), "2026-11-21");
        assert_eq!(store.day_key(at("2026-11-20T15:59:00Z")), "2026-11-20");
    }
}
