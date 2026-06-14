//! Engine 계층 — DB별 백업/복구 어댑터(PRD §10).
//!
//! - [`mongo`]: MongoDB 메타 질의(status·oplog ts) + mongodump/native 덤프 경로.
//! - [`native`]: Mongo 드라이버 네이티브 아카이브(`xb-native-v1`).
//! - [`postgres`]: PostgreSQL 드라이버 백업/복구/증분/PITR(`xb-pg-v1` 풀 + `xb-pg-incr-v1`
//!   증분, logical decoding, 2차).
//!
//! 전면 `Engine` trait 대신 핸들러 레벨에서 DB 종류([`crate::engine::DbKind`])로 분기하고,
//! 덤프/복구/status에만 얇은 seam을 둔다(작동하는 Mongo 코드의 전면 재작성 회피).

pub mod mongo;
pub mod native;
pub mod postgres;

/// 백업 대상 DB 종류 — source URI 스킴으로 판별한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbKind {
    /// MongoDB(`mongodb://`, `mongodb+srv://`).
    Mongo,
    /// PostgreSQL(`postgres://`, `postgresql://`).
    Postgres,
}

impl DbKind {
    /// URI 스킴으로 DB 종류를 판별한다. 인식 못 하면 Mongo로 본다(1차 기본).
    pub fn from_uri(uri: &str) -> Self {
        let lower = uri.trim_start().to_ascii_lowercase();
        if lower.starts_with("postgres://") || lower.starts_with("postgresql://") {
            DbKind::Postgres
        } else {
            DbKind::Mongo
        }
    }

    /// 사람용 표시 라벨(컨텍스트 줄·doctor 출력 등).
    pub fn label(self) -> &'static str {
        match self {
            DbKind::Postgres => "postgresql",
            DbKind::Mongo => "mongodb",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn db_kind_from_uri_scheme() {
        assert_eq!(DbKind::from_uri("postgresql://u:p@h/db"), DbKind::Postgres);
        assert_eq!(DbKind::from_uri("postgres://h/db"), DbKind::Postgres);
        assert_eq!(
            DbKind::from_uri("mongodb://h/?replicaSet=rs0"),
            DbKind::Mongo
        );
        assert_eq!(DbKind::from_uri("mongodb+srv://h/db"), DbKind::Mongo);
    }
}
