//! 엔진 중립 행 수 비교 — 백업이 가진 테이블(컬렉션)별 행 수와 대상 DB의 행 수를 판정한다
//! (`verify --against`).
//!
//! 엔진마다 세는 방법만 다르다(MySQL·PostgreSQL은 `COUNT(*)`, MongoDB는 `countDocuments`).
//! 판정 로직과 결과 타입은 이 모듈이 공유한다. 행 수 일치는 내용 일치를 보증하지 않는다
//! (누락·초과만 잡는다).

use std::collections::{BTreeMap, BTreeSet};

/// 한 테이블의 비교 결과 분류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableStatus {
    Match,
    Mismatch,
    /// 백업에는 있으나 대상 DB에 테이블이 없다.
    MissingInTarget,
    /// 대상 DB에만 있다(행 수는 세지 않는다).
    ExtraInTarget,
    /// 행 수는 같지만 내용 다이제스트가 다르다(`--content`).
    ContentMismatch,
    /// 행 수는 같지만 다이제스트가 한쪽에 없어 내용을 비교하지 못했다(`--content`).
    ContentUnchecked,
}

impl TableStatus {
    /// 출력용 토큰(항상 영문).
    pub fn label(self) -> &'static str {
        match self {
            Self::Match => "MATCH",
            Self::Mismatch => "MISMATCH",
            Self::MissingInTarget => "MISSING",
            Self::ExtraInTarget => "EXTRA",
            Self::ContentMismatch => "CONTENT",
            Self::ContentUnchecked => "NO-DIGEST",
        }
    }
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
    /// 대상 행 수가 추정치(`--estimate`)라 정확하지 않을 수 있는지.
    pub estimated: bool,
    /// 행 수가 같은 항목의 내용 다이제스트까지 대조했는지(`--content`).
    pub content_checked: bool,
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
    CompareReport {
        tables,
        estimated: false,
        content_checked: false,
    }
}

/// 행 수가 같은(`Match`) 항목 중 내용 다이제스트가 다른 것을 `ContentMismatch`로 바꾼다(순수).
///
/// 어느 한쪽에 다이제스트가 없는 항목은 `ContentUnchecked`로 바꾼다. `--content`를 요청했는데
/// 내용을 보지 못한 테이블이 `MATCH`로 남으면 검증이 끝난 것처럼 보이기 때문이다.
pub fn apply_content_digests(
    report: &mut CompareReport,
    backup: &BTreeMap<String, String>,
    target: &BTreeMap<String, String>,
) {
    for entry in &mut report.tables {
        if entry.status != TableStatus::Match {
            continue;
        }
        match (backup.get(&entry.table), target.get(&entry.table)) {
            (Some(b), Some(t)) if b != t => entry.status = TableStatus::ContentMismatch,
            (Some(_), Some(_)) => {}
            _ => entry.status = TableStatus::ContentUnchecked,
        }
    }
    report.content_checked = true;
}

fn content_unsupported() -> crate::error::XBackupError {
    crate::error::XBackupError::Usage(crate::tr!(
        "--content supports only MariaDB/MySQL backups for now",
        "--content는 아직 MariaDB/MySQL 백업만 지원합니다"
    ))
}

/// 비교를 지원하는 백업 엔진(아카이브 포맷으로 구분한다).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareEngine {
    Mysql,
    Postgres,
    MongoNative,
}

impl CompareEngine {
    /// manifest의 `archive_format`으로 엔진을 고른다. 지원하지 않으면 None.
    pub fn from_archive_format(format: Option<&str>) -> Option<Self> {
        match format? {
            f if f == crate::engine::mysql::archive::FORMAT_ID => Some(Self::Mysql),
            f if f == crate::engine::postgres::archive::FORMAT_ID => Some(Self::Postgres),
            f if f == crate::engine::native::archive::FORMAT_ID => Some(Self::MongoNative),
            _ => None,
        }
    }

    /// 대상 URI의 종류가 이 엔진과 맞는지.
    pub fn matches_target(self, kind: crate::engine::DbKind) -> bool {
        use crate::engine::DbKind;
        matches!(
            (self, kind),
            (Self::Mysql, DbKind::Mysql)
                | (Self::Postgres, DbKind::Postgres)
                | (Self::MongoNative, DbKind::Mongo)
        )
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Mysql => "MariaDB/MySQL",
            Self::Postgres => "PostgreSQL",
            Self::MongoNative => "MongoDB",
        }
    }
}

/// 대상 DB에 순차로 접속해 이름 목록을 얻고 항목별 행 수를 세는 연결.
pub enum LiveTarget {
    Mysql(crate::engine::mysql::conn::MysqlClient),
    Postgres {
        client: crate::engine::postgres::conn::PgClient,
        live: crate::engine::postgres::compare::LiveTables,
    },
    Mongo {
        client: mongodb::Client,
        estimate: bool,
    },
}

/// 대상에서 읽은 항목 이름과, 백업 쪽 행 수에서 빼야 하는 이름.
#[derive(Debug, Default)]
pub struct LiveNames {
    pub names: BTreeSet<String>,
    /// 백업 아카이브에는 있으나 비교 대상이 아닌 항목(PostgreSQL 파티션 부모).
    pub ignore_in_backup: BTreeSet<String>,
}

impl LiveTarget {
    /// 대상에 접속한다. `estimate`는 MongoDB 대상에만 적용한다.
    pub async fn connect(
        engine: CompareEngine,
        uri: &crate::config::secret::Secret,
        estimate: bool,
    ) -> crate::error::Result<Self> {
        if estimate && engine != CompareEngine::MongoNative {
            return Err(crate::error::XBackupError::Usage(crate::tr!(
                "--estimate applies only to MongoDB targets",
                "--estimate는 MongoDB 대상에만 적용합니다"
            )));
        }
        Ok(match engine {
            CompareEngine::Mysql => {
                Self::Mysql(crate::engine::mysql::conn::MysqlClient::connect(uri, None).await?)
            }
            CompareEngine::Postgres => Self::Postgres {
                client: crate::engine::postgres::conn::PgClient::connect(uri, None).await?,
                live: Default::default(),
            },
            CompareEngine::MongoNative => Self::Mongo {
                client: crate::engine::native::compare::connect(uri).await?,
                estimate,
            },
        })
    }

    /// 비교 대상 항목 이름을 나열한다.
    pub async fn names(&mut self) -> crate::error::Result<LiveNames> {
        match self {
            Self::Mysql(client) => Ok(LiveNames {
                names: crate::engine::mysql::compare::live_table_names(client.conn_mut()).await?,
                ..Default::default()
            }),
            Self::Postgres { client, live } => {
                *live = crate::engine::postgres::compare::live_tables(client.client()).await?;
                Ok(LiveNames {
                    names: live.tables.keys().cloned().collect(),
                    ignore_in_backup: live.partitioned_parents.clone(),
                })
            }
            Self::Mongo { client, .. } => Ok(LiveNames {
                names: crate::engine::native::compare::live_namespaces(client).await?,
                ..Default::default()
            }),
        }
    }

    /// 내용 다이제스트 스캔을 준비한다(백업과 같은 세션·일관 스냅샷). MariaDB/MySQL만 지원한다.
    pub async fn prepare_content_scan(&mut self) -> crate::error::Result<()> {
        match self {
            Self::Mysql(client) => {
                crate::engine::mysql::digest::prepare_scan_session(client.conn_mut()).await
            }
            _ => Err(content_unsupported()),
        }
    }

    /// 항목 하나의 내용 다이제스트를 대상에서 읽어 구한다. MariaDB/MySQL만 지원한다.
    pub async fn digest(&mut self, name: &str) -> crate::error::Result<String> {
        match self {
            Self::Mysql(client) => {
                use mysql_async::prelude::Queryable;
                let conn = client.conn_mut();
                let database: Option<Option<String>> =
                    conn.query_first("SELECT DATABASE()").await.map_err(|e| {
                        crate::error::XBackupError::Failure(crate::tr!(
                            "failed to read the target database: {e}",
                            "대상 데이터베이스 조회 실패: {e}"
                        ))
                    })?;
                let db = database.flatten().ok_or_else(|| {
                    crate::error::XBackupError::Usage(crate::tr!(
                        "the comparison target URI must name a database (mysql://.../<db>)",
                        "비교 대상 URI에 데이터베이스가 필요합니다(mysql://.../<db>)"
                    ))
                })?;
                let (_rows, digest) =
                    crate::engine::mysql::digest::scan_table_digest(conn, &db, name).await?;
                Ok(digest)
            }
            _ => Err(content_unsupported()),
        }
    }

    /// 항목 하나의 행(문서) 수를 센다.
    pub async fn count(&mut self, name: &str) -> crate::error::Result<u64> {
        match self {
            Self::Mysql(client) => {
                crate::engine::mysql::compare::count_live_rows(client.conn_mut(), name).await
            }
            Self::Postgres { client, live } => {
                let (schema, table) = live.tables.get(name).ok_or_else(|| {
                    crate::error::XBackupError::Failure(crate::tr!(
                        "unknown PostgreSQL table: '{name}'",
                        "알 수 없는 PostgreSQL 테이블: '{name}'"
                    ))
                })?;
                crate::engine::postgres::compare::count_live_rows(client.client(), schema, table)
                    .await
            }
            Self::Mongo { client, estimate } => {
                crate::engine::native::compare::count_live_docs(client, name, *estimate).await
            }
        }
    }
}

/// 백업의 항목별 행 수를 대상 DB와 비교한다. 대상은 항목마다 순차로 센다.
///
/// - `only`가 비어 있지 않으면 그 항목만 비교하고, 백업과 대상 어디에도 없는 이름은 사용법 오류다.
/// - `report_extra`가 거짓이면 대상에만 있는 항목(`EXTRA`)은 보고하지 않는다(복구는 백업에 없는
///   기존 테이블을 지우지 않으므로, 복구 직후 확인에서는 잡음이다).
pub async fn compare_with_target(
    engine: CompareEngine,
    target: &crate::config::secret::Secret,
    mut backup_counts: BTreeMap<String, u64>,
    only: &[String],
    estimate: bool,
    report_extra: bool,
    content: Option<&crate::manifest::schema::ContentDigests>,
) -> crate::error::Result<CompareReport> {
    // 계산 방식을 모르는 다이제스트는 해석하지 않는다 — 접속하기 전에 거부한다.
    if let Some(digests) = content {
        if digests.algorithm != crate::engine::mysql::digest::ALGORITHM {
            return Err(crate::error::XBackupError::Usage(crate::tr!(
                "unsupported content digest algorithm '{}' (this build understands '{}')",
                "지원하지 않는 내용 다이제스트 알고리즘 '{}'입니다(이 빌드는 '{}'만 이해합니다)",
                digests.algorithm,
                crate::engine::mysql::digest::ALGORITHM
            )));
        }
    }

    let mut live_target = LiveTarget::connect(engine, target, estimate).await?;
    let live = live_target.names().await?;
    backup_counts.retain(|name, _| !live.ignore_in_backup.contains(name));

    if !only.is_empty() {
        let unknown: Vec<&str> = only
            .iter()
            .filter(|t| !backup_counts.contains_key(*t) && !live.names.contains(*t))
            .map(String::as_str)
            .collect();
        if !unknown.is_empty() {
            return Err(crate::error::XBackupError::Usage(crate::tr!(
                "--tables names entries that exist neither in the backup nor in the target: {}",
                "--tables에 백업과 대상 어디에도 없는 항목이 있습니다: {}",
                unknown.join(", ")
            )));
        }
        backup_counts.retain(|table, _| only.contains(table));
    }

    let mut target_counts = BTreeMap::new();
    for table in backup_counts.keys().filter(|t| live.names.contains(*t)) {
        let rows = live_target.count(table).await?;
        target_counts.insert(table.clone(), rows);
    }
    let extra: BTreeSet<String> = if report_extra {
        live.names
            .iter()
            .filter(|t| !backup_counts.contains_key(*t) && (only.is_empty() || only.contains(t)))
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };
    let mut report = compare_counts(&backup_counts, &target_counts, &extra);
    report.estimated = estimate;

    // 내용 다이제스트 — 행 수가 이미 어긋난 항목은 읽지 않는다(전체 스캔이라 비싸다).
    if let Some(digests) = content {
        live_target.prepare_content_scan().await?;
        let mut target_digests = BTreeMap::new();
        for entry in report
            .tables
            .iter()
            .filter(|t| t.status == TableStatus::Match)
        {
            let digest = live_target.digest(&entry.table).await?;
            target_digests.insert(entry.table.clone(), digest);
        }
        apply_content_digests(&mut report, &digests.tables, &target_digests);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn engine_is_chosen_by_archive_format() {
        use crate::engine::{mysql, native, postgres};
        assert_eq!(
            CompareEngine::from_archive_format(Some(mysql::archive::FORMAT_ID)),
            Some(CompareEngine::Mysql)
        );
        assert_eq!(
            CompareEngine::from_archive_format(Some(postgres::archive::FORMAT_ID)),
            Some(CompareEngine::Postgres)
        );
        assert_eq!(
            CompareEngine::from_archive_format(Some(native::archive::FORMAT_ID)),
            Some(CompareEngine::MongoNative)
        );
        assert_eq!(CompareEngine::from_archive_format(Some("0.1")), None);
        assert_eq!(CompareEngine::from_archive_format(None), None);
    }

    #[test]
    fn target_kind_must_match_the_backup_engine() {
        use crate::engine::DbKind;
        assert!(CompareEngine::Mysql.matches_target(DbKind::Mysql));
        assert!(CompareEngine::Postgres.matches_target(DbKind::Postgres));
        assert!(CompareEngine::MongoNative.matches_target(DbKind::Mongo));
        assert!(!CompareEngine::Mysql.matches_target(DbKind::Postgres));
        assert!(!CompareEngine::Postgres.matches_target(DbKind::Mongo));
        assert!(!CompareEngine::MongoNative.matches_target(DbKind::File));
    }

    fn digests(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn report_of(rows: &[(&str, u64, u64)]) -> CompareReport {
        let backup: BTreeMap<String, u64> =
            rows.iter().map(|(t, b, _)| (t.to_string(), *b)).collect();
        let target: BTreeMap<String, u64> =
            rows.iter().map(|(t, _, n)| (t.to_string(), *n)).collect();
        compare_counts(&backup, &target, &BTreeSet::new())
    }

    #[test]
    fn content_mismatch_is_flagged_only_for_tables_whose_counts_match() {
        // a: 행 수 일치 + 다이제스트 다름 → CONTENT, b: 행 수 일치 + 같음 → MATCH, c: 행 수가 이미 다름 → 그대로
        let mut report = report_of(&[("a", 3, 3), ("b", 2, 2), ("c", 5, 4)]);
        apply_content_digests(
            &mut report,
            &digests(&[("a", "11"), ("b", "22"), ("c", "33")]),
            &digests(&[("a", "99"), ("b", "22"), ("c", "99")]),
        );
        let status = |name: &str| {
            report
                .tables
                .iter()
                .find(|t| t.table == name)
                .unwrap()
                .status
        };
        assert_eq!(status("a"), TableStatus::ContentMismatch);
        assert_eq!(status("b"), TableStatus::Match);
        assert_eq!(
            status("c"),
            TableStatus::Mismatch,
            "행 수 불일치는 내용 비교로 덮지 않는다"
        );
        assert!(report.content_checked);
        assert!(!report.is_match());
    }

    #[test]
    fn a_table_missing_a_digest_on_either_side_is_not_reported_as_match() {
        let mut report = report_of(&[("a", 1, 1), ("b", 1, 1), ("c", 1, 1)]);
        apply_content_digests(
            &mut report,
            &digests(&[("a", "11"), ("c", "33")]),
            &digests(&[("b", "22"), ("c", "33")]),
        );
        let status = |name: &str| {
            report
                .tables
                .iter()
                .find(|t| t.table == name)
                .unwrap()
                .status
        };
        assert_eq!(status("a"), TableStatus::ContentUnchecked);
        assert_eq!(status("b"), TableStatus::ContentUnchecked);
        assert_eq!(status("c"), TableStatus::Match);
        assert!(
            !report.is_match(),
            "내용을 보지 못한 테이블이 있으면 일치가 아니다"
        );
    }

    #[test]
    fn content_statuses_have_their_own_labels() {
        assert_eq!(TableStatus::ContentMismatch.label(), "CONTENT");
        assert_eq!(TableStatus::ContentUnchecked.label(), "NO-DIGEST");
    }

    #[tokio::test]
    async fn an_unknown_digest_algorithm_is_rejected_before_connecting() {
        let digests = crate::manifest::schema::ContentDigests {
            algorithm: "other-algo-v9".to_string(),
            tables: BTreeMap::new(),
        };
        // 포트 1은 닫혀 있다 — 접속을 시도했다면 연결 오류(exit 1)가 났을 것이다.
        let target = crate::config::secret::Secret::new("mysql://u:p@127.0.0.1:1/db".to_string());
        let err = compare_with_target(
            CompareEngine::Mysql,
            &target,
            BTreeMap::new(),
            &[],
            false,
            true,
            Some(&digests),
        )
        .await
        .unwrap_err();
        assert_eq!(err.exit_code(), 2, "{err}");
        assert!(err.to_string().contains("other-algo-v9"), "{err}");
    }
}
