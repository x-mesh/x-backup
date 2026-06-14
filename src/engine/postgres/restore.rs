//! PostgreSQL 네이티브 복구 — [`archive`](super::archive) 스트림을 드라이버(COPY)로 복원한다.
//!
//! 프레임을 하나씩 읽어:
//! - `Q`(시퀀스): `CREATE SEQUENCE IF NOT EXISTS` (테이블 DEFAULT의 nextval 해소). last_value는
//!   마지막에 `setval`로 복원.
//! - `T`(테이블): 비-기본 스키마 `CREATE SCHEMA`, (`drop`이면 `DROP ... CASCADE`) `CREATE TABLE`.
//!   제약·인덱스는 모든 데이터 적재 후로 **지연 적용**(FK 참조 테이블이 다 존재하도록).
//! - `D`/`X`: `COPY <t>(copy_cols) FROM STDIN (FORMAT text)`로 불투명 바이트를 그대로 적재
//!   (STORED generated 컬럼은 copy_cols에서 제외 — 자동 재계산).
//!
//! 제약·인덱스를 데이터 뒤로 미루므로 COPY 중 FK/트리거 이슈가 없다(별도 권한 불필요).
//! IDENTITY 컬럼은 적재 후 시퀀스를 max로 리셋한다.

use bytes::Bytes;
use futures::SinkExt;
use tokio::io::AsyncRead;
use tokio_postgres::Client;

use super::archive::{self, Frame};
use super::conn::PgClient;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 대상 URI로 연결해 아카이브 스트림을 복원한다. 반환은 삽입한 총 행 수.
pub async fn pg_restore<R: AsyncRead + Unpin>(
    reader: &mut R,
    target_uri: &Secret,
    timeout_secs: Option<u64>,
    drop: bool,
) -> Result<u64> {
    let pg = PgClient::connect(target_uri, timeout_secs).await?;
    restore_into(reader, pg.client(), drop).await
}

/// 이미 연결된 클라이언트로 복원한다(migrate 등에서 재사용).
pub async fn restore_into<R: AsyncRead + Unpin>(
    reader: &mut R,
    client: &Client,
    drop: bool,
) -> Result<u64> {
    // 함수 본문이 아직 없는 테이블을 참조해도 생성 단계에서 실패하지 않도록 검증을 끈다
    //   (pg_dump 복구와 동일). 세션 한정.
    let _ = client
        .batch_execute("SET check_function_bodies = false")
        .await;

    // 헤더 확인 + 메이저 버전 정합 경고(text COPY라 보통 호환되나, 메이저 차이는 알린다).
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(format!(
                    "PG 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
            warn_on_major_mismatch(client, h.get_str("pg_version").unwrap_or("")).await;
        }
        other => {
            return Err(XBackupError::Failure(format!(
                "PG 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    let mut inserted = 0u64;
    let mut deferred_constraints: Vec<String> = Vec::new();
    let mut deferred_indexes: Vec<String> = Vec::new();
    let mut setvals: Vec<(String, i64, bool)> = Vec::new();
    // IDENTITY 컬럼 (ns, 컬럼평문이름) — 적재 후 시퀀스를 max로 리셋(리뷰 #4).
    let mut identity_resets: Vec<(String, String)> = Vec::new();
    // 선행 DDL(확장·타입) 버퍼 — 첫 시퀀스/테이블 전에 재시도 적용. 후행 DDL(뷰)은 맨 끝.
    let mut pre_ddls: Vec<String> = Vec::new();
    let mut post_ddls: Vec<String> = Vec::new();
    let mut pre_applied = false;

    loop {
        match archive::read_frame(reader).await? {
            Frame::Header(_) => {
                return Err(XBackupError::Failure(
                    "PG 아카이브 헤더가 중복됩니다".into(),
                ))
            }
            Frame::Pre(d) => {
                if let Ok(sql) = d.get_str("sql") {
                    pre_ddls.push(sql.to_string());
                }
            }
            Frame::Post(d) => {
                if let Ok(sql) = d.get_str("sql") {
                    post_ddls.push(sql.to_string());
                }
            }
            Frame::Sequence(s) => {
                // 시퀀스/테이블 전에 선행 DDL(확장·타입)을 의존성 순서대로 적용한다.
                if !pre_applied {
                    apply_with_retry(client, &pre_ddls, "선행 DDL(확장/타입)").await?;
                    pre_applied = true;
                }
                let name = s
                    .get_str("name")
                    .map_err(|_| XBackupError::Failure("시퀀스 프레임에 name이 없습니다".into()))?;
                let last_value = s.get_i64("last_value").unwrap_or(1);
                let is_called = s.get_bool("is_called").unwrap_or(true);
                // 비-기본 스키마는 시퀀스 전에 만든다(시퀀스가 그 스키마에 속할 수 있음).
                let schema = s.get_str("schema").unwrap_or("");
                if !schema.is_empty() {
                    run_ignore_exists(client, &format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                        .await
                        .map_err(|e| {
                            XBackupError::Failure(format!("{schema} 스키마 생성 실패: {e}"))
                        })?;
                }
                // 파라미터를 보존한 CREATE SEQUENCE DDL(없으면 기본 생성).
                let create = s
                    .get_str("create_sql")
                    .map(String::from)
                    .unwrap_or_else(|_| format!("CREATE SEQUENCE {name}"));
                run_ignore_exists(client, &create)
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{name} 시퀀스 생성 실패: {e}")))?;
                setvals.push((name.to_string(), last_value, is_called));
            }
            Frame::Table(meta) => {
                if !pre_applied {
                    apply_with_retry(client, &pre_ddls, "선행 DDL(확장/타입)").await?;
                    pre_applied = true;
                }
                let ns = meta
                    .get_str("ns")
                    .map_err(|_| XBackupError::Failure("테이블 프레임에 ns가 없습니다".into()))?
                    .to_string();
                let create_sql = meta.get_str("create_sql").map_err(|_| {
                    XBackupError::Failure(format!("{ns} 테이블에 create_sql이 없습니다"))
                })?;
                // 식별자는 백업이 정확히 quote해 프레임에 담아둔다(create_sql 재파싱 불필요 — 리뷰 #13).
                let quoted = meta.get_str("quoted").unwrap_or(&ns).to_string();
                let schema = meta.get_str("schema").unwrap_or("");

                // 비-기본 스키마는 테이블 전에 만든다(없으면 CREATE TABLE 실패 — 리뷰 #5).
                if !schema.is_empty() {
                    run_ignore_exists(client, &format!("CREATE SCHEMA IF NOT EXISTS {schema}"))
                        .await
                        .map_err(|e| {
                            XBackupError::Failure(format!("{schema} 스키마 생성 실패: {e}"))
                        })?;
                }

                if drop {
                    let _ = client
                        .batch_execute(&format!("DROP TABLE IF EXISTS {quoted} CASCADE"))
                        .await;
                }
                // 테이블 생성(이미 있으면 drop=false 경로 — append로 허용).
                run_ignore_exists(client, create_sql)
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{ns} 테이블 생성 실패: {e}")))?;

                // 제약·인덱스는 데이터 적재 후로 지연.
                if let Ok(arr) = meta.get_array("constraints") {
                    deferred_constraints
                        .extend(arr.iter().filter_map(|b| b.as_str().map(String::from)));
                }
                if let Ok(arr) = meta.get_array("indexes") {
                    deferred_indexes
                        .extend(arr.iter().filter_map(|b| b.as_str().map(String::from)));
                }
                // IDENTITY 컬럼은 데이터 적재 후 시퀀스를 max로 리셋(평문 이름 보존 — 리뷰 #4).
                let id_cols: Vec<String> = meta
                    .get_array("identity_cols")
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| b.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                for col in &id_cols {
                    identity_resets.push((ns.clone(), col.clone()));
                }

                // COPY 컬럼 목록 — STORED generated 제외(백업이 정한 copy_cols, 리뷰 #3).
                let copy_cols: Vec<String> = meta
                    .get_array("copy_cols")
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| b.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let col_list = if copy_cols.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", copy_cols.join(", "))
                };

                // COPY IN — 이 테이블의 Data*를 TableEnd까지 적재(text — 백업과 동일 포맷).
                let copy_sql = format!("COPY {quoted}{col_list} FROM STDIN (FORMAT text)");
                let sink = client
                    .copy_in(copy_sql.as_str())
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{ns} COPY IN 시작 실패: {e}")))?;
                futures::pin_mut!(sink);
                loop {
                    match archive::read_frame(reader).await? {
                        Frame::Data(bytes) => {
                            sink.send(Bytes::from(bytes)).await.map_err(|e| {
                                XBackupError::Failure(format!("{ns} COPY 데이터 전송 실패: {e}"))
                            })?;
                        }
                        Frame::TableEnd => break,
                        other => {
                            return Err(XBackupError::Failure(format!(
                                "{ns} 데이터 중 예기치 못한 프레임: {other:?}"
                            )))
                        }
                    }
                }
                let n = sink
                    .finish()
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{ns} COPY IN 종료 실패: {e}")))?;
                inserted += n;
            }
            Frame::Data(_) | Frame::TableEnd => {
                return Err(XBackupError::Failure(
                    "테이블 밖에서 데이터 프레임을 만났습니다(손상)".into(),
                ))
            }
            Frame::End => break,
        }
    }

    // 지연 적용: 제약 → 인덱스(이미 있으면 무시) → 시퀀스 setval.
    for sql in &deferred_constraints {
        run_ignore_exists(client, sql)
            .await
            .map_err(|e| XBackupError::Failure(format!("제약 적용 실패: {e}\n  SQL: {sql}")))?;
    }
    for sql in &deferred_indexes {
        run_ignore_exists(client, sql)
            .await
            .map_err(|e| XBackupError::Failure(format!("인덱스 적용 실패: {e}\n  SQL: {sql}")))?;
    }
    for (name, last_value, is_called) in &setvals {
        // 식별자를 바인드 파라미터로 — 문자열 리터럴 인젝션 방지(리뷰 #1). name은 이미
        // 정확히 quote된 전체 식별자이므로 text→regclass 캐스트로 안전하게 해석된다.
        // $1::text::regclass — 안쪽 ::text로 파라미터를 text로 강제(드라이버가 regclass를
        // 직렬화 못 하는 문제 회피). name은 정확히 quote된 식별자라 regclass 해석이 안전.
        if let Err(e) = client
            .execute(
                "SELECT setval($1::text::regclass, $2, $3)",
                &[name, last_value, is_called],
            )
            .await
        {
            tracing::warn!(sequence = %name, "시퀀스 setval 실패: {e}");
        }
    }
    // IDENTITY 컬럼의 (재생성된) 내부 시퀀스를 현재 max로 리셋 — 이후 INSERT 충돌 방지(리뷰 #4).
    for (ns, col) in &identity_resets {
        let (schema, table) = match ns.split_once('.') {
            Some(p) => p,
            None => continue,
        };
        let quoted: String = match client
            .query_one(
                "SELECT format('%I.%I', $1::text, $2::text)",
                &[&schema, &table],
            )
            .await
        {
            Ok(r) => r.get(0),
            Err(_) => continue,
        };
        // pg_get_serial_sequence는 IDENTITY 컬럼에도 동작한다(실측 확인).
        let _ = client
            .execute(
                &format!(
                    "SELECT setval(pg_get_serial_sequence($1, $2), \
                     (SELECT COALESCE(MAX({}), 0) FROM {quoted}), true)",
                    quote_col(col)
                ),
                &[&ns, &col],
            )
            .await;
    }

    // 선행 DDL이 아직 안 돌았으면(시퀀스·테이블이 하나도 없는 백업) 여기서 적용.
    if !pre_applied {
        apply_with_retry(client, &pre_ddls, "선행 DDL(확장/타입)").await?;
    }
    // 후행 DDL(뷰·머티리얼라이즈드뷰) — 테이블·데이터가 모두 준비된 뒤 재시도 적용.
    apply_with_retry(client, &post_ddls, "후행 DDL(뷰)").await?;

    Ok(inserted)
}

/// DDL 묶음을 의존성 순서에 무관하게 적용한다 — 매 라운드 남은 것을 시도하고 실패는 모아 재시도,
/// 진전이 없으면 마지막 에러로 중단. 뷰·타입 간 상호 의존(순서 문제)을 흡수한다.
async fn apply_with_retry(client: &Client, ddls: &[String], what: &str) -> Result<()> {
    let mut pending: Vec<&String> = ddls.iter().collect();
    while !pending.is_empty() {
        let mut still: Vec<&String> = Vec::new();
        let mut last_err: Option<String> = None;
        for sql in &pending {
            match run_ignore_exists(client, sql).await {
                Ok(()) => {}
                Err(e) => {
                    last_err = Some(format!("{e}\n  SQL: {sql}"));
                    still.push(sql);
                }
            }
        }
        // 한 라운드에서 하나도 못 줄였으면 순환/진짜 오류 — 중단.
        if still.len() == pending.len() {
            return Err(XBackupError::Failure(format!(
                "{what} 적용 실패(의존성 해소 불가): {}",
                last_err.unwrap_or_default()
            )));
        }
        pending = still;
    }
    Ok(())
}

/// 백업 소스와 복구 대상의 메이저 버전이 다르면 경고한다(차단하지 않음 — text COPY는 보통 호환).
async fn warn_on_major_mismatch(client: &Client, source_version: &str) {
    let target_version: String = match client.query_one("SHOW server_version", &[]).await {
        Ok(row) => row.get(0),
        Err(_) => return,
    };
    let major = |v: &str| v.trim().split('.').next().unwrap_or("").to_string();
    let (sm, tm) = (major(source_version), major(target_version.as_str()));
    if !sm.is_empty() && !tm.is_empty() && sm != tm {
        tracing::warn!(
            "PostgreSQL 메이저 버전 불일치 — 백업 소스={source_version}, 복구 대상={target_version}. \
             text COPY라 대개 호환되나 타입·기본값 차이를 복구 후 확인하세요"
        );
    }
}

/// 이미 존재(중복) 오류는 무시하고 실행한다(drop=false 재적용·idempotent 경로).
async fn run_ignore_exists(
    client: &Client,
    sql: &str,
) -> std::result::Result<(), tokio_postgres::Error> {
    match client.batch_execute(sql).await {
        Ok(_) => Ok(()),
        Err(e) => {
            // "이미 존재"(객체 중복)만 무시한다. UNIQUE_VIOLATION(데이터가 제약 위반)은
            // 절대 삼키지 않는다 — 무결성 실패를 숨기게 된다(리뷰 #1).
            let dup = e
                .code()
                .map(|c| {
                    matches!(
                        *c,
                        tokio_postgres::error::SqlState::DUPLICATE_TABLE
                            | tokio_postgres::error::SqlState::DUPLICATE_OBJECT
                            | tokio_postgres::error::SqlState::DUPLICATE_SCHEMA
                    )
                })
                .unwrap_or(false);
            if dup {
                tracing::debug!("이미 존재 — 건너뜀: {e}");
                Ok(())
            } else {
                Err(e)
            }
        }
    }
}

/// 컬럼 식별자를 표준 방식(쌍따옴표로 감싸고 내부 쌍따옴표는 두 배)으로 안전하게 quote한다.
fn quote_col(col: &str) -> String {
    format!("\"{}\"", col.replace('"', "\"\""))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_col_doubles_quotes() {
        assert_eq!(quote_col("id"), "\"id\"");
        assert_eq!(quote_col("we\"ird"), "\"we\"\"ird\"");
    }
}
