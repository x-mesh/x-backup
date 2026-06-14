//! PostgreSQL 읽기 전용 메타 질의 — peek·watch·migrate가 공유한다(외부 도구 없이 드라이버 SQL).
//!
//! Mongo의 [`MongoMeta`](crate::engine::mongo::MongoMeta) 대응 — 테이블 행 수(정확/추정),
//! DB 크기, 최신 행 미리보기.

use tokio_postgres::Client;

use super::conn::PgClient;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 시스템 스키마(사용자 데이터에서 제외) — 엔진 전반 동일 기준.
const SYSTEM_SCHEMAS: &str = "'pg_catalog','information_schema','pg_toast'";

/// URI로 연결한다(공유 [`PgClient`]).
pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<PgClient> {
    PgClient::connect(uri, timeout_secs).await
}

/// 사용자 테이블 (schema.table) 목록(정렬).
async fn list_tables(
    client: &Client,
    schema_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let sql = format!(
        "SELECT schemaname, tablename FROM pg_tables \
         WHERE schemaname NOT IN ({SYSTEM_SCHEMAS}) \
         AND ($1::text IS NULL OR schemaname = $1) ORDER BY schemaname, tablename"
    );
    let rows = client
        .query(sql.as_str(), &[&schema_filter])
        .await
        .map_err(|e| XBackupError::Failure(format!("테이블 목록 조회 실패: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, String>(1)))
        .collect())
}

/// 사용자 테이블별 **정확한** 행 수(`count(*)`) — peek·migrate 계획(정확도 우선, 1회성).
pub async fn table_counts_exact(client: &Client) -> Result<Vec<(String, u64)>> {
    let tables = list_tables(client, None).await?;
    let mut out = Vec::with_capacity(tables.len());
    for (schema, table) in tables {
        // 식별자를 서버에서 정확히 quote해 인젝션·특수문자 안전.
        let quoted: String = client
            .query_one(
                "SELECT format('%I.%I', $1::text, $2::text)",
                &[&schema, &table],
            )
            .await
            .map(|r| r.get(0))
            .map_err(|e| {
                XBackupError::Failure(format!("{schema}.{table} 식별자 조회 실패: {e}"))
            })?;
        let count: i64 = client
            .query_one(&format!("SELECT count(*) FROM {quoted}"), &[])
            .await
            .map(|r| r.get(0))
            .map_err(|e| XBackupError::Failure(format!("{schema}.{table} count 실패: {e}")))?;
        out.push((format!("{schema}.{table}"), count.max(0) as u64));
    }
    Ok(out)
}

/// 사용자 테이블별 **추정** 행 수(`reltuples`) — watch(매 틱, 저비용). ANALYZE 전이면 0일 수 있음.
pub async fn table_counts_estimated(client: &Client) -> Result<Vec<(String, u64)>> {
    let sql = format!(
        "SELECT n.nspname || '.' || c.relname, greatest(0, c.reltuples)::bigint \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname NOT IN ({SYSTEM_SCHEMAS}) \
         ORDER BY 1"
    );
    let rows = client
        .query(sql.as_str(), &[])
        .await
        .map_err(|e| XBackupError::Failure(format!("행 수 추정 조회 실패: {e}")))?;
    Ok(rows
        .into_iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1).max(0) as u64))
        .collect())
}

/// 현재 데이터베이스의 논리 크기(바이트) — `pg_database_size`.
pub async fn data_size_bytes(client: &Client) -> Result<u64> {
    let sz: i64 = client
        .query_one("SELECT pg_database_size(current_database())::bigint", &[])
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::Failure(format!("DB 크기 조회 실패: {e}")))?;
    Ok(sz.max(0) as u64)
}

/// 한 테이블의 최신 N행을 JSON 문자열로 반환한다(peek 데이터 육안 확인).
///
/// `ns`는 `schema.table`. 물리 순서 역순(`ctid DESC`)으로 "가장 최근 삽입에 가까운" 행을 본다
/// (append 패턴 가정 — 정확한 시간순은 아님). 각 행은 `to_jsonb` 텍스트.
pub async fn latest_rows(client: &Client, ns: &str, limit: i64) -> Result<Vec<String>> {
    let (schema, table) = ns.split_once('.').ok_or_else(|| {
        XBackupError::Usage(format!("네임스페이스 형식 오류: '{ns}'(schema.table)"))
    })?;
    let quoted: String = client
        .query_one(
            "SELECT format('%I.%I', $1::text, $2::text)",
            &[&schema, &table],
        )
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::Failure(format!("{ns} 식별자 조회 실패: {e}")))?;
    let sql =
        format!("SELECT to_jsonb(t)::text FROM {quoted} t ORDER BY t.ctid DESC LIMIT {limit}");
    let rows = client
        .query(sql.as_str(), &[])
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 최신 행 조회 실패: {e}")))?;
    Ok(rows.into_iter().map(|r| r.get::<_, String>(0)).collect())
}
