//! MySQL 읽기 전용 메타 질의 — peek·watch·migrate가 공유한다(외부 도구 없이 드라이버 SQL).
//!
//! PG의 [`meta`](crate::engine::postgres::meta) 대응 — 테이블 행 수(정확/추정), DB 크기,
//! 최신 행 미리보기. `mysql_async`는 `&mut Conn`을 요구하므로 PG의 공유 `&Client`와 달리
//! 가변 참조를 받는다.

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row, Value};

use super::conn::MysqlClient;
use super::util::quote_qualified;
use super::value::ColCategory;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// URI로 연결한다(공유 [`MysqlClient`]).
pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<MysqlClient> {
    MysqlClient::connect(uri, timeout_secs).await
}

/// 현재 데이터베이스 이름.
pub async fn current_db(conn: &mut Conn) -> Result<String> {
    conn.query_first::<Option<String>, _>("SELECT DATABASE()")
        .await
        .map_err(|e| XBackupError::Failure(format!("현재 데이터베이스 조회 실패: {e}")))?
        .flatten()
        .ok_or_else(|| {
            XBackupError::Usage("URI에 데이터베이스가 지정되어야 합니다(mysql://.../<db>)".into())
        })
}

/// 현재 DB의 BASE TABLE 이름 목록(정렬).
async fn list_tables(conn: &mut Conn) -> Result<Vec<String>> {
    conn.query::<String, _>(
        "SELECT TABLE_NAME FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY TABLE_NAME",
    )
    .await
    .map_err(|e| XBackupError::Failure(format!("테이블 목록 조회 실패: {e}")))
}

/// 사용자 테이블의 `db.table` 목록 — migrate 충돌 감지용(실패는 하드 에러로 전파).
pub async fn list_qualified(conn: &mut Conn) -> Result<Vec<String>> {
    let db = current_db(conn).await?;
    Ok(list_tables(conn)
        .await?
        .into_iter()
        .map(|t| format!("{db}.{t}"))
        .collect())
}

/// 사용자 테이블별 **정확한** 행 수(`COUNT(*)`) — peek·migrate 계획(정확도 우선, 1회성).
pub async fn table_counts_exact(conn: &mut Conn) -> Result<Vec<(String, u64)>> {
    let db = current_db(conn).await?;
    let tables = list_tables(conn).await?;
    let mut out = Vec::with_capacity(tables.len());
    for table in tables {
        let quoted = quote_qualified(&db, &table);
        let count: u64 = conn
            .query_first(format!("SELECT COUNT(*) FROM {quoted}"))
            .await
            .map_err(|e| XBackupError::Failure(format!("{db}.{table} count 실패: {e}")))?
            .unwrap_or(0);
        out.push((format!("{db}.{table}"), count));
    }
    Ok(out)
}

/// 사용자 테이블별 **추정** 행 수(`information_schema.TABLES.TABLE_ROWS`) — watch(매 틱, 저비용).
/// InnoDB 추정치는 40~50% 오차가 있을 수 있다(정확값은 `table_counts_exact`).
pub async fn table_counts_estimated(conn: &mut Conn) -> Result<Vec<(String, u64)>> {
    conn.query::<(String, u64), _>(
        "SELECT CONCAT(TABLE_SCHEMA, '.', TABLE_NAME), COALESCE(TABLE_ROWS, 0) \
         FROM information_schema.TABLES \
         WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE' ORDER BY 1",
    )
    .await
    .map_err(|e| XBackupError::Failure(format!("행 수 추정 조회 실패: {e}")))
}

/// 현재 데이터베이스의 논리 크기(바이트) — `SUM(data_length + index_length)`.
pub async fn data_size_bytes(conn: &mut Conn) -> Result<u64> {
    let sz: Option<u64> = conn
        .query_first(
            "SELECT COALESCE(SUM(data_length + index_length), 0) \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("DB 크기 조회 실패: {e}")))?;
    Ok(sz.unwrap_or(0))
}

/// 한 테이블의 최신 N행을 JSON 문자열로 반환한다(peek 데이터 육안 확인).
///
/// `ns`는 `db.table`. PK가 있으면 그 역순(`PK DESC`)으로 "가장 최근 삽입에 가까운" 행을 본다
/// (AUTO_INCREMENT 가정 — 정확한 시간순은 아님). 바이너리는 `0x..`, 그 외는 문자열로 표현한다.
pub async fn latest_rows(conn: &mut Conn, ns: &str, limit: i64) -> Result<Vec<String>> {
    let (db, table) = ns
        .split_once('.')
        .ok_or_else(|| XBackupError::Usage(format!("네임스페이스 형식 오류: '{ns}'(db.table)")))?;
    let quoted = quote_qualified(db, table);

    // 컬럼(이름 + 카테고리).
    let cols: Vec<(String, String)> = conn
        .exec(
            "SELECT COLUMN_NAME, DATA_TYPE FROM information_schema.COLUMNS \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? ORDER BY ORDINAL_POSITION",
            (db, table),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 컬럼 조회 실패: {e}")))?;
    if cols.is_empty() {
        return Ok(Vec::new());
    }
    let col_names: Vec<String> = cols.iter().map(|(n, _)| n.clone()).collect();
    let cats: Vec<ColCategory> = cols
        .iter()
        .map(|(_, dt)| ColCategory::from_data_type(dt))
        .collect();
    let select_list = col_names
        .iter()
        .map(|n| super::util::quote_ident(n))
        .collect::<Vec<_>>()
        .join(", ");

    // PK 컬럼(역순 정렬 키). 없으면 정렬 없이 LIMIT.
    let pk: Vec<String> = conn
        .exec(
            "SELECT COLUMN_NAME FROM information_schema.KEY_COLUMN_USAGE \
             WHERE TABLE_SCHEMA = ? AND TABLE_NAME = ? AND CONSTRAINT_NAME = 'PRIMARY' \
             ORDER BY ORDINAL_POSITION",
            (db, table),
        )
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} PK 조회 실패: {e}")))?;
    let order = if pk.is_empty() {
        String::new()
    } else {
        let keys = pk
            .iter()
            .map(|n| format!("{} DESC", super::util::quote_ident(n)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" ORDER BY {keys}")
    };

    let sql = format!("SELECT {select_list} FROM {quoted}{order} LIMIT {limit}");
    let rows: Vec<Row> = conn
        .query(sql)
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} 최신 행 조회 실패: {e}")))?;
    Ok(rows
        .iter()
        .map(|row| row_to_json(row, &col_names, &cats))
        .collect())
}

/// 한 행을 JSON 객체 문자열로(미리보기용). 바이너리는 `0x..`, 숫자는 숫자, 그 외는 문자열.
fn row_to_json(row: &Row, names: &[String], cats: &[ColCategory]) -> String {
    let mut map = serde_json::Map::new();
    for (i, name) in names.iter().enumerate() {
        let v = row.as_ref(i).unwrap_or(&Value::NULL);
        let jv = match v {
            Value::NULL => serde_json::Value::Null,
            Value::Bytes(b) => match cats.get(i) {
                Some(ColCategory::Binary) => {
                    serde_json::Value::String(format!("0x{}", hex::encode(b)))
                }
                Some(ColCategory::Numeric) => {
                    let s = String::from_utf8_lossy(b);
                    serde_json::from_str(&s).unwrap_or(serde_json::Value::String(s.into_owned()))
                }
                _ => serde_json::Value::String(String::from_utf8_lossy(b).into_owned()),
            },
            other => serde_json::Value::String(super::value::render_value(other, ColCategory::Text)),
        };
        map.insert(name.clone(), jv);
    }
    serde_json::Value::Object(map).to_string()
}
