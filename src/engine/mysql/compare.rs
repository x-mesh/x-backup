//! 백업 아카이브와 대상 DB의 테이블별 행 수 비교(`verify --against`).
//!
//! manifest에는 행 수가 없어 아카이브의 `T`/`D`/`X` 프레임을 끝까지 읽어 센다(상수 메모리).
//! 대상 DB는 테이블마다 `COUNT(*)`를 **하나씩 순차로** 실행한다 — InnoDB는 행 수를 저장하지
//! 않아 인덱스 전체를 훑으므로 병렬로 돌리면 운영 DB에 부하가 겹친다.
//!
//! 행 수 일치는 내용 일치를 보증하지 않는다(누락·초과만 잡는다).

use std::collections::{BTreeMap, BTreeSet};

use mysql_async::prelude::Queryable;
use mysql_async::Conn;
use tokio::io::AsyncRead;

use super::archive::{self, Frame};
use super::util::quote_ident;
use crate::error::{Result, XBackupError};

/// 한 테이블의 비교 결과 분류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStatus {
    Match,
    Mismatch,
    /// 백업에는 있으나 대상 DB에 테이블이 없다.
    MissingInTarget,
    /// 대상 DB에만 있다(행 수는 세지 않는다).
    ExtraInTarget,
}

/// 테이블 하나의 백업·대상 행 수와 판정.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableCompare {
    pub table: String,
    pub backup_rows: Option<u64>,
    pub target_rows: Option<u64>,
    pub status: TableStatus,
}

/// 전체 비교 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompareReport {
    pub tables: Vec<TableCompare>,
}

impl CompareReport {
    pub fn is_match(&self) -> bool {
        self.tables.iter().all(|t| t.status == TableStatus::Match)
    }

    pub fn mismatched(&self) -> impl Iterator<Item = &TableCompare> {
        self.tables
            .iter()
            .filter(|t| t.status != TableStatus::Match)
    }
}

/// `db.table` ns에서 테이블 이름만 뽑는다(복구 대상 DB 이름은 백업과 다를 수 있다).
fn table_name(ns: &str) -> &str {
    ns.split_once('.').map_or(ns, |(_, table)| table)
}

/// 아카이브 스트림을 끝까지 읽어 테이블별 행 수를 센다.
pub async fn count_archive_rows<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<BTreeMap<String, u64>> {
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(crate::tr!(
                    "MySQL archive format mismatch: '{fmt}' (expected '{}')",
                    "MySQL 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
        }
        other => {
            return Err(XBackupError::Failure(crate::tr!(
                "MySQL archive header missing (first frame: {other:?})",
                "MySQL 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    let mut counts = BTreeMap::new();
    let mut current: Option<String> = None;
    loop {
        match archive::read_frame(reader).await? {
            Frame::Table(doc) => {
                let ns = doc.get_str("ns").unwrap_or("");
                current = Some(table_name(ns).to_string());
                counts.entry(table_name(ns).to_string()).or_insert(0u64);
            }
            Frame::Data(_) => {
                let Some(table) = current.as_ref() else {
                    return Err(XBackupError::Failure(crate::tr!(
                        "MySQL archive row frame appeared before any table frame",
                        "MySQL 아카이브에서 테이블 프레임 없이 행 프레임이 나왔습니다"
                    )));
                };
                if let Some(n) = counts.get_mut(table) {
                    *n += 1;
                }
            }
            Frame::TableEnd => current = None,
            Frame::End => return Ok(counts),
            Frame::Header(_) | Frame::Pre(_) | Frame::Post(_) => {}
        }
    }
}

/// 연결된 DB(`DATABASE()`)의 일반 테이블 이름 목록(뷰 제외).
pub async fn live_table_names(conn: &mut Conn) -> Result<BTreeSet<String>> {
    let database: Option<Option<String>> =
        conn.query_first("SELECT DATABASE()").await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to read the target database: {e}",
                "대상 데이터베이스 조회 실패: {e}"
            ))
        })?;
    if database.flatten().is_none() {
        return Err(XBackupError::Usage(crate::tr!(
            "the comparison target URI must name a database (mysql://.../<db>)",
            "비교 대상 URI에 데이터베이스가 필요합니다(mysql://.../<db>)"
        )));
    }
    let names: Vec<String> = conn
        .query(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = DATABASE() AND table_type = 'BASE TABLE'",
        )
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to list target tables: {e}",
                "대상 테이블 목록 조회 실패: {e}"
            ))
        })?;
    Ok(names.into_iter().collect())
}

/// 연결된 DB의 테이블 하나의 행 수(`COUNT(*)`).
pub async fn count_live_rows(conn: &mut Conn, table: &str) -> Result<u64> {
    let quoted = quote_ident(table);
    let count: Option<u64> = conn
        .query_first(format!("SELECT COUNT(*) FROM {quoted}"))
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to count rows of {table}: {e}",
                "{table} 행 수 조회 실패: {e}"
            ))
        })?;
    Ok(count.unwrap_or(0))
}

/// 백업 행 수와 대상 행 수를 판정한다(순수).
///
/// `target_counts`는 실제로 센 테이블만, `extra_in_target`은 백업에 없는 대상 테이블이다.
pub fn compare_counts(
    backup: &BTreeMap<String, u64>,
    target_counts: &BTreeMap<String, u64>,
    extra_in_target: &BTreeSet<String>,
) -> CompareReport {
    let mut tables = Vec::new();
    for (table, &backup_rows) in backup {
        let (target_rows, status) = match target_counts.get(table) {
            Some(&rows) if rows == backup_rows => (Some(rows), TableStatus::Match),
            Some(&rows) => (Some(rows), TableStatus::Mismatch),
            None => (None, TableStatus::MissingInTarget),
        };
        tables.push(TableCompare {
            table: table.clone(),
            backup_rows: Some(backup_rows),
            target_rows,
            status,
        });
    }
    for table in extra_in_target {
        tables.push(TableCompare {
            table: table.clone(),
            backup_rows: None,
            target_rows: None,
            status: TableStatus::ExtraInTarget,
        });
    }
    CompareReport { tables }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mysql::archive::{
        write_end, write_header, write_row, write_table, write_table_end, HeaderFrame, TableFrame,
    };

    async fn archive_with(tables: &[(&str, usize)]) -> Vec<u8> {
        let mut buf = Vec::new();
        write_header(
            &mut buf,
            &HeaderFrame {
                created_at: "2026-10-28T00:00:00Z",
                mysql_version: "10.2.25-MariaDB",
                database_charset: "utf8mb4",
                database_collation: "utf8mb4_general_ci",
                binlog_file: "",
                binlog_pos: 0,
                gtid_executed: "",
            },
        )
        .await
        .unwrap();
        for (ns, rows) in tables {
            write_table(
                &mut buf,
                &TableFrame {
                    ns,
                    quoted: "`t`",
                    create_sql: "CREATE TABLE t (id int)",
                    insert_cols: &["`id`".to_string()],
                },
            )
            .await
            .unwrap();
            for i in 0..*rows {
                write_row(&mut buf, format!("({i})").as_bytes())
                    .await
                    .unwrap();
            }
            write_table_end(&mut buf).await.unwrap();
        }
        write_end(&mut buf).await.unwrap();
        buf
    }

    #[tokio::test]
    async fn counts_rows_per_table_ignoring_database_prefix() {
        let buf = archive_with(&[("intranet.g5_board", 3), ("intranet.empty", 0)]).await;
        let counts = count_archive_rows(&mut &buf[..]).await.unwrap();
        assert_eq!(counts.get("g5_board"), Some(&3));
        assert_eq!(counts.get("empty"), Some(&0));
        assert_eq!(counts.len(), 2);
    }

    #[tokio::test]
    async fn rejects_archive_without_header() {
        let mut buf = Vec::new();
        write_end(&mut buf).await.unwrap();
        assert!(count_archive_rows(&mut &buf[..]).await.is_err());
    }

    #[test]
    fn classifies_match_mismatch_missing_and_extra() {
        let backup = BTreeMap::from([
            ("a".to_string(), 3u64),
            ("b".to_string(), 5),
            ("c".to_string(), 1),
        ]);
        let target = BTreeMap::from([("a".to_string(), 3u64), ("b".to_string(), 4)]);
        let extra = BTreeSet::from(["z".to_string()]);

        let report = compare_counts(&backup, &target, &extra);
        let status = |name: &str| {
            report
                .tables
                .iter()
                .find(|t| t.table == name)
                .unwrap()
                .status
        };
        assert_eq!(status("a"), TableStatus::Match);
        assert_eq!(status("b"), TableStatus::Mismatch);
        assert_eq!(status("c"), TableStatus::MissingInTarget);
        assert_eq!(status("z"), TableStatus::ExtraInTarget);
        assert!(!report.is_match());
        assert_eq!(report.mismatched().count(), 3);
    }

    #[test]
    fn identical_counts_match() {
        let backup = BTreeMap::from([("a".to_string(), 2u64)]);
        let report = compare_counts(&backup, &backup.clone(), &BTreeSet::new());
        assert!(report.is_match());
    }
}
