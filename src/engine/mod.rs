//! Engine 계층 — DB별 백업/복구 어댑터(PRD §10).
//!
//! - [`mongo`]: MongoDB 메타 질의(status·oplog ts) + mongodump/native 덤프 경로.
//! - [`native`]: Mongo 드라이버 네이티브 아카이브(`xb-native-v1`).
//! - [`postgres`]: PostgreSQL 드라이버 백업/복구/증분/PITR(`xb-pg-v1` 풀 + `xb-pg-incr-v1`
//!   증분, logical decoding, 2차).
//! - [`mysql`]: MySQL/MariaDB 드라이버 백업/복구/증분/PITR(`xb-mysql-v1` 풀 +
//!   `xb-mysql-incr-v1` 증분, binlog ROW 디코드, 3차).
//!
//! 전면 `Engine` trait 대신 핸들러 레벨에서 DB 종류([`crate::engine::DbKind`])로 분기하고,
//! 덤프/복구/status에만 얇은 seam을 둔다(작동하는 Mongo 코드의 전면 재작성 회피).

pub mod mongo;
pub mod mysql;
pub mod native;
pub mod postgres;

/// 백업 대상 DB 종류 — source URI 스킴으로 판별한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    /// MongoDB(`mongodb://`, `mongodb+srv://`).
    Mongo,
    /// PostgreSQL(`postgres://`, `postgresql://`).
    Postgres,
    /// MySQL·MariaDB(`mysql://`, `mariadb://`).
    Mysql,
}

impl DbKind {
    /// URI 스킴으로 DB 종류를 판별한다. 인식 못 하면 Mongo로 본다(1차 기본).
    pub fn from_uri(uri: &str) -> Self {
        let lower = uri.trim_start().to_ascii_lowercase();
        if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            DbKind::Postgres
        } else if lower.starts_with("mysql://") || lower.starts_with("mariadb://") {
            DbKind::Mysql
        } else {
            DbKind::Mongo
        }
    }

    /// 사람용 표시 라벨(컨텍스트 줄·doctor 출력 등).
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgresql",
            DbKind::Mongo => "mongodb",
            DbKind::Mysql => "mysql",
        }
    }

    /// manifest의 `archive_format`으로 백업을 만든 엔진의 DB 종류를 판별한다
    /// (표시·필터용 — list/picker의 단일 진실 원천, Phase 1 슬라이스 A).
    ///
    /// 풀·증분 포맷을 모두 프리픽스로 인식한다(`xb-pg-v1`/`xb-pg-incr-v1` 등).
    /// 미기록(구버전)·`mongodump`·`xb-native-v1`은 모두 Mongo다(1차 기본과 동일한 관대함).
    /// 복구 파이프라인의 소비자 선택은 정확한 FORMAT_ID 매칭을 유지한다
    /// ([`crate::pipeline::restore`]) — 여기는 종류 판별만 담당한다.
    pub fn from_archive_format(fmt: Option<&str>) -> Self {
        match fmt {
            Some(f) if f.starts_with("xb-pg") => DbKind::Postgres,
            Some(f) if f.starts_with("xb-mysql") => DbKind::Mysql,
            _ => DbKind::Mongo,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_kind_from_archive_format_covers_full_and_incr() {
        // 풀/증분 포맷 프리픽스 인식 + 미기록·레거시는 Mongo.
        assert_eq!(
            DbKind::from_archive_format(Some("xb-pg-v1")),
            DbKind::Postgres
        );
        assert_eq!(
            DbKind::from_archive_format(Some("xb-pg-incr-v1")),
            DbKind::Postgres
        );
        assert_eq!(
            DbKind::from_archive_format(Some("xb-mysql-v1")),
            DbKind::Mysql
        );
        assert_eq!(
            DbKind::from_archive_format(Some("xb-mysql-incr-v1")),
            DbKind::Mysql
        );
        assert_eq!(
            DbKind::from_archive_format(Some("xb-native-v1")),
            DbKind::Mongo
        );
        assert_eq!(
            DbKind::from_archive_format(Some("mongodump")),
            DbKind::Mongo
        );
        assert_eq!(DbKind::from_archive_format(None), DbKind::Mongo);
    }

    #[test]
    fn db_kind_from_uri_scheme() {
        assert_eq!(DbKind::from_uri("postgresql://u:p@h/db"), DbKind::Postgres);
        assert_eq!(DbKind::from_uri("postgres://h/db"), DbKind::Postgres);
        assert_eq!(
            DbKind::from_uri("mongodb://h/?replicaSet=rs0"),
            DbKind::Mongo
        );
        assert_eq!(DbKind::from_uri("mongodb+srv://h/db"), DbKind::Mongo);
        assert_eq!(DbKind::from_uri("mysql://u:p@h:3306/db"), DbKind::Mysql);
        assert_eq!(DbKind::from_uri("mariadb://h/db"), DbKind::Mysql);
    }
}
