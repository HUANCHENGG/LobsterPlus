//! 本地代理每日用量统计只读查询。

use chrono::{Duration, Local, NaiveDate};
use rusqlite::{params, Connection, OpenFlags};
use serde::Serialize;
use std::path::Path;
use std::time::Duration as StdDuration;

const USAGE_TABLE: &str = "usage_daily";
const USAGE_COLUMNS: [&str; 11] = [
    "day",
    "protocol",
    "model",
    "input",
    "output",
    "request",
    "success",
    "error",
    "accurate",
    "estimated",
    "missing",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UsageRange {
    Today,
    SevenDays,
    ThirtyDays,
    All,
}

impl UsageRange {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "today" => Ok(Self::Today),
            "7d" => Ok(Self::SevenDays),
            "30d" => Ok(Self::ThirtyDays),
            "all" => Ok(Self::All),
            _ => Err(format!("无效的用量统计范围：{value}")),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Today => "today",
            Self::SevenDays => "7d",
            Self::ThirtyDays => "30d",
            Self::All => "all",
        }
    }

    fn start(self, today: NaiveDate) -> Option<NaiveDate> {
        match self {
            Self::Today => Some(today),
            Self::SevenDays => Some(today - Duration::days(6)),
            Self::ThirtyDays => Some(today - Duration::days(29)),
            Self::All => None,
        }
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageTotals {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub requests: u64,
    pub successes: u64,
    pub errors: u64,
    pub accurate_requests: u64,
    pub estimated_requests: u64,
    pub missing_requests: u64,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DailyUsage {
    pub day: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelUsage {
    pub model: String,
    #[serde(flatten)]
    pub totals: UsageTotals,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageRangeInfo {
    pub range: String,
    pub start_day: Option<String>,
    pub end_day: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageReport {
    pub range: UsageRangeInfo,
    pub total: UsageTotals,
    pub daily: Vec<DailyUsage>,
    pub models: Vec<ModelUsage>,
}

impl UsageReport {
    fn empty(range: UsageRange, today: NaiveDate) -> Self {
        Self {
            range: UsageRangeInfo {
                range: range.name().to_string(),
                start_day: range.start(today).map(|d| d.format("%Y-%m-%d").to_string()),
                end_day: today.format("%Y-%m-%d").to_string(),
            },
            total: UsageTotals::default(),
            daily: Vec::new(),
            models: Vec::new(),
        }
    }
}

pub fn query(db: &Path, range: &str) -> Result<UsageReport, String> {
    query_on(db, UsageRange::parse(range)?, Local::now().date_naive())
}

fn query_on(db: &Path, range: UsageRange, today: NaiveDate) -> Result<UsageReport, String> {
    let mut report = UsageReport::empty(range, today);
    if !db.is_file() {
        return Ok(report);
    }

    let conn = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .map_err(|e| format!("打开用量数据库失败 {}: {e}", db.display()))?;
    conn.busy_timeout(StdDuration::from_secs(5)).ok();
    validate_schema(&conn)?;

    let start = range.start(today).map(|d| d.format("%Y-%m-%d").to_string());
    let end = today.format("%Y-%m-%d").to_string();
    let where_clause = if start.is_some() {
        "day >= ?1 AND day <= ?2"
    } else {
        "day <= ?2"
    };

    report.total = query_totals(&conn, where_clause, start.as_deref(), &end)?;
    report.daily = query_daily(&conn, where_clause, start.as_deref(), &end)?;
    report.models = query_models(&conn, where_clause, start.as_deref(), &end)?;
    if range == UsageRange::All {
        report.range.start_day = report.daily.first().map(|row| row.day.clone());
    }
    Ok(report)
}

fn validate_schema(conn: &Connection) -> Result<(), String> {
    let mut stmt = conn
        .prepare("PRAGMA table_info(usage_daily)")
        .map_err(schema_error)?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))
        .map_err(schema_error)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(schema_error)?;
    if columns == USAGE_COLUMNS {
        Ok(())
    } else {
        Err(format!(
            "用量数据库 schema 不兼容：usage_daily 列应为 [{}]，实际为 [{}]",
            USAGE_COLUMNS.join(","),
            columns.join(",")
        ))
    }
}

fn schema_error(error: rusqlite::Error) -> String {
    format!("检查用量数据库 schema 失败：{error}")
}

fn bind_and_query<'a>(
    stmt: &'a mut rusqlite::Statement<'_>,
    start: Option<&str>,
    end: &str,
) -> rusqlite::Result<rusqlite::Rows<'a>> {
    match start {
        Some(start) => stmt.query(params![start, end]),
        None => stmt.query(params![rusqlite::types::Null, end]),
    }
}

fn totals_from_row(row: &rusqlite::Row<'_>, offset: usize) -> rusqlite::Result<UsageTotals> {
    let input_tokens = row.get(offset)?;
    let output_tokens = row.get(offset + 1)?;
    Ok(UsageTotals {
        input_tokens,
        output_tokens,
        total_tokens: input_tokens.saturating_add(output_tokens),
        requests: row.get(offset + 2)?,
        successes: row.get(offset + 3)?,
        errors: row.get(offset + 4)?,
        accurate_requests: row.get(offset + 5)?,
        estimated_requests: row.get(offset + 6)?,
        missing_requests: row.get(offset + 7)?,
    })
}

fn aggregate_columns() -> &'static str {
    "COALESCE(SUM(input), 0), COALESCE(SUM(output), 0), \
     COALESCE(SUM(request), 0), COALESCE(SUM(success), 0), \
     COALESCE(SUM(error), 0), COALESCE(SUM(accurate), 0), \
     COALESCE(SUM(estimated), 0), COALESCE(SUM(missing), 0)"
}

fn query_totals(
    conn: &Connection,
    where_clause: &str,
    start: Option<&str>,
    end: &str,
) -> Result<UsageTotals, String> {
    let sql = format!(
        "SELECT {} FROM {USAGE_TABLE} WHERE {where_clause}",
        aggregate_columns()
    );
    let mut stmt = conn.prepare(&sql).map_err(query_error)?;
    let mut rows = bind_and_query(&mut stmt, start, end).map_err(query_error)?;
    let row = rows
        .next()
        .map_err(query_error)?
        .ok_or_else(|| "查询用量总计失败".to_string())?;
    totals_from_row(row, 0).map_err(query_error)
}

fn query_daily(
    conn: &Connection,
    where_clause: &str,
    start: Option<&str>,
    end: &str,
) -> Result<Vec<DailyUsage>, String> {
    let sql = format!(
        "SELECT day, {} FROM {USAGE_TABLE} WHERE {where_clause} GROUP BY day ORDER BY day ASC",
        aggregate_columns()
    );
    let mut stmt = conn.prepare(&sql).map_err(query_error)?;
    let rows = bind_and_query(&mut stmt, start, end).map_err(query_error)?;
    rows.mapped(|row| {
        Ok(DailyUsage {
            day: row.get(0)?,
            totals: totals_from_row(row, 1)?,
        })
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(query_error)
}

fn query_models(
    conn: &Connection,
    where_clause: &str,
    start: Option<&str>,
    end: &str,
) -> Result<Vec<ModelUsage>, String> {
    let sql = format!(
        "SELECT model, {} FROM {USAGE_TABLE} WHERE {where_clause} \
         GROUP BY model ORDER BY (SUM(input) + SUM(output)) DESC, model ASC",
        aggregate_columns()
    );
    let mut stmt = conn.prepare(&sql).map_err(query_error)?;
    let rows = bind_and_query(&mut stmt, start, end).map_err(query_error)?;
    rows.mapped(|row| {
        Ok(ModelUsage {
            model: row.get(0)?,
            totals: totals_from_row(row, 1)?,
        })
    })
    .collect::<Result<Vec<_>, _>>()
    .map_err(query_error)
}

fn query_error(error: rusqlite::Error) -> String {
    format!("查询用量数据库失败：{error}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("lp-usage-{name}-{}", uuid::Uuid::new_v4()))
    }

    const PYTHON_USAGE_CREATE_SQL: &str = "CREATE TABLE IF NOT EXISTS usage_daily (
        day TEXT NOT NULL,
        protocol TEXT NOT NULL,
        model TEXT NOT NULL,
        input INTEGER NOT NULL DEFAULT 0,
        output INTEGER NOT NULL DEFAULT 0,
        request INTEGER NOT NULL DEFAULT 0,
        success INTEGER NOT NULL DEFAULT 0,
        error INTEGER NOT NULL DEFAULT 0,
        accurate INTEGER NOT NULL DEFAULT 0,
        estimated INTEGER NOT NULL DEFAULT 0,
        missing INTEGER NOT NULL DEFAULT 0,
        PRIMARY KEY (day, protocol, model)
    )";

    fn make_db(rows: &[(&str, &str, &str, u64, u64, u64, u64, u64, u64, u64)]) -> PathBuf {
        let dir = temp_path("db");
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("usage.sqlite");
        let mut conn = Connection::open(&db).unwrap();
        conn.execute_batch(PYTHON_USAGE_CREATE_SQL).unwrap();
        let tx = conn.transaction().unwrap();
        for row in rows {
            tx.execute(
                "INSERT INTO usage_daily
                 (day, protocol, model, input, output, request, success, error, accurate, estimated, missing)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10],
            )
            .unwrap();
        }
        tx.commit().unwrap();
        db
    }

    fn day(value: &str) -> NaiveDate {
        NaiveDate::parse_from_str(value, "%Y-%m-%d").unwrap()
    }

    #[test]
    fn missing_database_returns_empty_report() {
        let db = temp_path("missing").join("usage.sqlite");
        let report = query_on(&db, UsageRange::Today, day("2026-10-10")).unwrap();
        assert_eq!(report.total, UsageTotals::default());
        assert!(report.daily.is_empty());
        assert!(report.models.is_empty());
        assert_eq!(report.range.start_day.as_deref(), Some("2026-10-10"));
    }

    #[test]
    fn validates_and_applies_ranges() {
        assert!(UsageRange::parse("week").is_err());
        let db = make_db(&[
            ("2026-09-10", "openai", "old", 1, 1, 1, 1, 0, 1, 0, 0),
            ("2026-10-04", "openai", "seven", 2, 3, 1, 1, 0, 1, 0, 0),
            ("2026-10-10", "openai", "today", 5, 8, 1, 1, 0, 1, 0, 0),
        ]);
        let today = query_on(&db, UsageRange::Today, day("2026-10-10")).unwrap();
        assert_eq!(today.total.total_tokens, 13);
        let seven = query_on(&db, UsageRange::SevenDays, day("2026-10-10")).unwrap();
        assert_eq!(seven.total.total_tokens, 18);
        assert_eq!(seven.range.start_day.as_deref(), Some("2026-10-04"));
        let thirty = query_on(&db, UsageRange::ThirtyDays, day("2026-10-10")).unwrap();
        assert_eq!(thirty.total.total_tokens, 18);
        let all = query_on(&db, UsageRange::All, day("2026-10-10")).unwrap();
        assert_eq!(all.total.total_tokens, 20);
        assert_eq!(all.range.start_day.as_deref(), Some("2026-09-10"));
        assert_eq!(all.range.end_day, "2026-10-10");
        assert_eq!(all.daily.len(), 3);
        assert_eq!(all.models.len(), 3);
        fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    #[test]
    fn aggregates_across_days_protocols_and_models() {
        let db = make_db(&[
            ("2026-10-09", "openai", "alpha", 10, 4, 1, 1, 0, 1, 0, 0),
            ("2026-10-09", "anthropic", "alpha", 5, 3, 2, 1, 1, 0, 1, 1),
            ("2026-10-10", "openai", "beta", 7, 9, 3, 2, 1, 1, 1, 1),
        ]);
        let report = query_on(&db, UsageRange::SevenDays, day("2026-10-10")).unwrap();
        assert_eq!(
            (report.total.input_tokens, report.total.output_tokens),
            (22, 16)
        );
        assert_eq!(
            (
                report.total.requests,
                report.total.successes,
                report.total.errors
            ),
            (6, 4, 2)
        );
        assert_eq!(report.daily.len(), 2);
        assert_eq!(report.daily[0].totals.total_tokens, 22);
        assert_eq!(report.models.len(), 2);
        assert_eq!(report.models[0].model, "alpha");
        assert_eq!(report.models[0].totals.requests, 3);
        assert_eq!(report.models[1].model, "beta");
        fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    #[test]
    fn keeps_usage_source_classification() {
        let db = make_db(&[("2026-10-10", "openai", "alpha", 10, 5, 5, 4, 1, 2, 2, 1)]);
        let report = query_on(&db, UsageRange::Today, day("2026-10-10")).unwrap();
        assert_eq!(report.total.accurate_requests, 2);
        assert_eq!(report.total.estimated_requests, 2);
        assert_eq!(report.total.missing_requests, 1);
        fs::remove_dir_all(db.parent().unwrap()).unwrap();
    }

    #[test]
    fn rejects_old_or_incompatible_schema() {
        let dir = temp_path("old-schema");
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("usage.sqlite");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_daily (
                day TEXT NOT NULL,
                protocol TEXT NOT NULL,
                model TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0
            );",
        )
        .unwrap();
        drop(conn);
        let error = query_on(&db, UsageRange::All, day("2026-10-10")).unwrap_err();
        assert!(error.contains("schema 不兼容"));
        assert!(error.contains("input_tokens"));
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_database_returns_error() {
        let dir = temp_path("corrupt");
        fs::create_dir_all(&dir).unwrap();
        let db = dir.join("usage.sqlite");
        fs::write(&db, b"not sqlite").unwrap();
        let error = query_on(&db, UsageRange::Today, day("2026-10-10")).unwrap_err();
        assert!(error.contains("用量数据库"));
        fs::remove_dir_all(dir).unwrap();
    }
}
