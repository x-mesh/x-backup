//! PostgreSQL status 점검 — 연결·버전·DB 크기·테이블/행 수를 SQL로 조회한다(읽기 전용).
//!
//! Mongo status와 동일한 [`StatusReport`]/[`CheckItem`] 모델을 재사용하므로 `status`·`status
//! --all` 비교 뷰가 그대로 동작한다(키가 겹치는 항목은 source/target diff로 비교된다).

use super::conn::PgClient;
use crate::config::secret::Secret;
use crate::engine::mongo::status::{human_bytes, CheckItem, StatusReport};

const SYSTEM_SCHEMAS: &str = "'pg_catalog','information_schema','pg_toast'";

/// PostgreSQL 프로파일의 status 보고서를 만든다(연결 실패도 보고서로 표현).
pub async fn full_report(profile: &str, uri: &Secret, timeout_secs: Option<u64>) -> StatusReport {
    let pg = match PgClient::connect(uri, timeout_secs).await {
        Ok(p) => p,
        Err(e) => {
            return StatusReport::new(
                profile,
                vec![
                    CheckItem::fail("connection", "연결·인증", format!("연결 실패: {e}"))
                        .with_value("연결 실패"),
                ],
            )
        }
    };
    let c = pg.client();
    let mut items =
        vec![CheckItem::ok("connection", "연결·인증", "연결 성공(PostgreSQL)").with_value("OK")];

    // 현재 데이터베이스 + 버전.
    if let Ok(row) = c.query_one("SELECT current_database()", &[]).await {
        let db: String = row.get(0);
        items.push(
            CheckItem::ok("database", "데이터베이스", format!("현재 DB: {db}")).with_value(db),
        );
    }
    if let Ok(row) = c.query_one("SHOW server_version", &[]).await {
        let v: String = row.get(0);
        items.push(CheckItem::ok("version", "버전", format!("PostgreSQL {v}")).with_value(v));
    }

    // DB 크기.
    if let Ok(row) = c
        .query_one("SELECT pg_database_size(current_database())::bigint", &[])
        .await
    {
        let sz: i64 = row.get(0);
        items.push(
            CheckItem::ok(
                "estimated_size",
                "예상 크기",
                format!("DB 크기 {}", human_bytes(sz)),
            )
            .with_value(human_bytes(sz)),
        );
    }

    // 테이블 수 + 추정 행 수(reltuples — 전수 스캔 없이).
    let count_sql = format!(
        "SELECT count(*)::bigint FROM pg_tables WHERE schemaname NOT IN ({SYSTEM_SCHEMAS})"
    );
    if let Ok(row) = c.query_one(count_sql.as_str(), &[]).await {
        let n: i64 = row.get(0);
        items.push(
            CheckItem::ok(
                "collection_count",
                "테이블 수",
                format!("사용자 테이블 {n}개"),
            )
            .with_value(n.to_string()),
        );
    }
    // reltuples는 ANALYZE 전이면 -1일 수 있어 GREATEST로 음수를 막는다(리뷰 #7).
    let rows_sql = format!(
        "SELECT greatest(0, coalesce(sum(c.reltuples), 0))::bigint \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname NOT IN ({SYSTEM_SCHEMAS})"
    );
    if let Ok(row) = c.query_one(rows_sql.as_str(), &[]).await {
        let n: i64 = row.get(0);
        items.push(
            CheckItem::ok("doc_count", "행 수", format!("추정 {n}행(reltuples)"))
                .with_value(n.to_string()),
        );
    }

    StatusReport::new(profile, items)
}
