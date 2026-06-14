//! PostgreSQL 네이티브 복구 — [`archive`](super::archive) 스트림을 드라이버(COPY)로 복원한다.
//!
//! 프레임을 하나씩 읽어:
//! - `Q`(시퀀스): `CREATE SEQUENCE IF NOT EXISTS` (테이블 DEFAULT의 nextval 해소). last_value는
//!   마지막에 `setval`로 복원.
//! - `T`(테이블): (`drop`이면 `DROP ... CASCADE`) `CREATE TABLE`(컬럼만). 제약·인덱스는 모든
//!   데이터 적재 후로 **지연 적용**(FK 참조 테이블이 다 존재하도록).
//! - `D`/`X`: `COPY ... FROM STDIN (FORMAT binary)`로 불투명 바이트를 그대로 적재.
//!
//! 제약·인덱스를 데이터 뒤로 미루므로 COPY 중 FK/트리거 이슈가 없다(별도 권한 불필요).

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
    // 헤더 확인.
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(format!(
                    "PG 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
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
    let mut setvals: Vec<(String, i64)> = Vec::new();

    loop {
        match archive::read_frame(reader).await? {
            Frame::Header(_) => {
                return Err(XBackupError::Failure(
                    "PG 아카이브 헤더가 중복됩니다".into(),
                ))
            }
            Frame::Sequence(s) => {
                let name = s
                    .get_str("name")
                    .map_err(|_| XBackupError::Failure("시퀀스 프레임에 name이 없습니다".into()))?;
                let last_value = s.get_i64("last_value").unwrap_or(1);
                run_ignore_exists(client, &format!("CREATE SEQUENCE {name}"))
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{name} 시퀀스 생성 실패: {e}")))?;
                setvals.push((name.to_string(), last_value));
            }
            Frame::Table(meta) => {
                let ns = meta
                    .get_str("ns")
                    .map_err(|_| XBackupError::Failure("테이블 프레임에 ns가 없습니다".into()))?
                    .to_string();
                let create_sql = meta.get_str("create_sql").map_err(|_| {
                    XBackupError::Failure(format!("{ns} 테이블에 create_sql이 없습니다"))
                })?;
                let quoted = quoted_from_create(create_sql).unwrap_or_else(|| ns.clone());

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

                // COPY IN — 이 테이블의 Data*를 TableEnd까지 적재.
                let copy_sql = format!("COPY {quoted} FROM STDIN (FORMAT binary)");
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
    for (name, last_value) in &setvals {
        // is_called=true로 복원 — 다음 nextval은 last_value+1.
        let _ = client
            .batch_execute(&format!(
                "SELECT setval('{name}'::regclass, {last_value}, true)"
            ))
            .await;
    }

    Ok(inserted)
}

/// 이미 존재(중복) 오류는 무시하고 실행한다(drop=false 재적용·idempotent 경로).
async fn run_ignore_exists(
    client: &Client,
    sql: &str,
) -> std::result::Result<(), tokio_postgres::Error> {
    match client.batch_execute(sql).await {
        Ok(_) => Ok(()),
        Err(e) => {
            let dup = e
                .code()
                .map(|c| {
                    matches!(
                        *c,
                        tokio_postgres::error::SqlState::DUPLICATE_TABLE
                            | tokio_postgres::error::SqlState::DUPLICATE_OBJECT
                            | tokio_postgres::error::SqlState::DUPLICATE_SCHEMA
                            | tokio_postgres::error::SqlState::UNIQUE_VIOLATION
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

/// `CREATE TABLE "sch"."tbl" (...)`에서 식별자 부분(`"sch"."tbl"`)을 뽑는다.
fn quoted_from_create(create_sql: &str) -> Option<String> {
    let rest = create_sql.strip_prefix("CREATE TABLE ")?;
    let end = rest.find(" (").or_else(|| rest.find('('))?;
    Some(rest[..end].trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoted_extraction() {
        assert_eq!(
            quoted_from_create("CREATE TABLE \"public\".\"t\" (\n  id integer\n)").as_deref(),
            Some("\"public\".\"t\"")
        );
    }
}
