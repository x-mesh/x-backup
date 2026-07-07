//! MySQL 네이티브 복구 — [`archive`](super::archive) 스트림을 드라이버 SQL로 복원한다(외부
//! mysql 클라이언트 불필요).
//!
//! 프레임을 하나씩 읽어:
//! - `T`(테이블): (`drop`이면 `DROP TABLE IF EXISTS`) `SHOW CREATE TABLE` DDL 실행(제약·인덱스·
//!   AUTO_INCREMENT 내장). FK 순서·순환은 세션 `FOREIGN_KEY_CHECKS=0`이 흡수(PG의
//!   `session_replication_role=replica` 등가) — 별도 지연 적용 불필요.
//! - `D`/`X`: 렌더링된 행 튜플을 다행 `INSERT`로 묶어 적재(max_allowed_packet 이하 청크).
//! - `O`(후행 DDL): 뷰·트리거·루틴·이벤트를 데이터 후 실행. `drop`이면 `DROP <kind> IF EXISTS`
//!   선행, 의존성 순서는 재시도로 흡수.
//!
//! 복구 세션은 mysqldump 헤더와 동등한 설정을 건다(`FOREIGN_KEY_CHECKS=0`, `UNIQUE_CHECKS=0`,
//! `SQL_MODE='NO_AUTO_VALUE_ON_ZERO'`, `time_zone='+00:00'`, `NAMES utf8mb4`).

use mysql_async::prelude::Queryable;
use mysql_async::Conn;
use tokio::io::AsyncRead;

use super::archive::{self, Frame};
use super::conn::MysqlClient;
use super::util::quote_ident;
use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 한 INSERT 청크의 대략적 상한(바이트) — max_allowed_packet 이하로 안전하게.
const FLUSH_BYTES: usize = 2_000_000;
/// 한 INSERT 청크의 최대 행 수.
const FLUSH_ROWS: usize = 2000;

/// 대상 URI로 연결해 아카이브 스트림을 복원한다. 반환은 삽입한 총 행 수.
pub async fn mysql_restore<R: AsyncRead + Unpin>(
    reader: &mut R,
    target_uri: &Secret,
    timeout_secs: Option<u64>,
    drop: bool,
) -> Result<u64> {
    let mut client = MysqlClient::connect(target_uri, timeout_secs).await?;
    restore_into(reader, client.conn_mut(), drop).await
}

/// 후행 DDL 객체(뷰·트리거·루틴·이벤트).
struct PostObj {
    kind: String,
    name: String,
    sql: String,
}

/// 이미 연결된 conn으로 복원한다(migrate 등에서 재사용).
pub async fn restore_into<R: AsyncRead + Unpin>(
    reader: &mut R,
    conn: &mut Conn,
    drop: bool,
) -> Result<u64> {
    // 세션 설정 — FK/UNIQUE 체크 해제(순서·순환 무관), permissive sql_mode, UTC, UTF-8.
    for stmt in [
        "SET FOREIGN_KEY_CHECKS = 0",
        "SET UNIQUE_CHECKS = 0",
        "SET SQL_MODE = 'NO_AUTO_VALUE_ON_ZERO'",
        "SET SESSION time_zone = '+00:00'",
        "SET NAMES utf8mb4",
    ] {
        conn.query_drop(stmt)
            .await
            .map_err(|e| XBackupError::Failure(format!("복구 세션 설정 실패({stmt}): {e}")))?;
    }

    // 헤더 확인 + 버전 정합 경고.
    match archive::read_frame(reader).await? {
        Frame::Header(h) => {
            let fmt = h.get_str("format").unwrap_or("");
            if fmt != archive::FORMAT_ID {
                return Err(XBackupError::Failure(format!(
                    "MySQL 아카이브 포맷 불일치: '{fmt}'(기대 '{}')",
                    archive::FORMAT_ID
                )));
            }
            warn_on_major_mismatch(conn, h.get_str("mysql_version").unwrap_or("")).await;
        }
        other => {
            return Err(XBackupError::Failure(format!(
                "MySQL 아카이브 헤더 누락(첫 프레임: {other:?})"
            )))
        }
    }

    let mut inserted = 0u64;
    let mut pre_ddls: Vec<String> = Vec::new();
    let mut post: Vec<PostObj> = Vec::new();

    loop {
        match archive::read_frame(reader).await? {
            Frame::Header(_) => {
                return Err(XBackupError::Failure(
                    "MySQL 아카이브 헤더가 중복됩니다".into(),
                ))
            }
            Frame::Pre(d) => {
                if let Ok(sql) = d.get_str("sql") {
                    pre_ddls.push(sql.to_string());
                }
            }
            Frame::Post(d) => {
                post.push(PostObj {
                    kind: d.get_str("kind").unwrap_or("").to_string(),
                    name: d.get_str("name").unwrap_or("").to_string(),
                    sql: d
                        .get_str("sql")
                        .map_err(|_| {
                            XBackupError::Failure("후행 DDL 프레임에 sql이 없습니다".into())
                        })?
                        .to_string(),
                });
            }
            Frame::Table(meta) => {
                let ns = meta.get_str("ns").unwrap_or("");
                let quoted = meta
                    .get_str("quoted")
                    .map_err(|_| XBackupError::Failure(format!("{ns} 테이블에 quoted가 없습니다")))?
                    .to_string();
                let create_sql = meta.get_str("create_sql").map_err(|_| {
                    XBackupError::Failure(format!("{ns} 테이블에 create_sql이 없습니다"))
                })?;
                let insert_cols: Vec<String> = meta
                    .get_array("insert_cols")
                    .map(|a| {
                        a.iter()
                            .filter_map(|b| b.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();

                if drop {
                    let _ = conn
                        .query_drop(format!("DROP TABLE IF EXISTS {quoted}"))
                        .await;
                }
                run_ignore_exists(conn, create_sql)
                    .await
                    .map_err(|e| XBackupError::Failure(format!("{ns} 테이블 생성 실패: {e}")))?;

                let col_list = if insert_cols.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", insert_cols.join(", "))
                };

                // 데이터 — 렌더링된 행 튜플을 다행 INSERT로 묶어 적재.
                let mut batch: Vec<String> = Vec::new();
                let mut batch_bytes = 0usize;
                loop {
                    match archive::read_frame(reader).await? {
                        Frame::Data(bytes) => {
                            let tuple = String::from_utf8(bytes).map_err(|e| {
                                XBackupError::Failure(format!("{ns} 행 튜플 디코드 실패: {e}"))
                            })?;
                            batch_bytes += tuple.len() + 1;
                            batch.push(tuple);
                            if batch_bytes >= FLUSH_BYTES || batch.len() >= FLUSH_ROWS {
                                inserted +=
                                    flush_insert(conn, &quoted, &col_list, &mut batch, ns).await?;
                                batch_bytes = 0;
                            }
                        }
                        Frame::TableEnd => {
                            inserted +=
                                flush_insert(conn, &quoted, &col_list, &mut batch, ns).await?;
                            break;
                        }
                        other => {
                            return Err(XBackupError::Failure(format!(
                                "{ns} 데이터 중 예기치 못한 프레임: {other:?}"
                            )))
                        }
                    }
                }
            }
            Frame::Data(_) | Frame::TableEnd => {
                return Err(XBackupError::Failure(
                    "테이블 밖에서 데이터 프레임을 만났습니다(손상)".into(),
                ))
            }
            Frame::End => break,
        }
    }

    // 선행 DDL(현재 미사용) best-effort 적용.
    for sql in &pre_ddls {
        let _ = run_ignore_exists(conn, sql).await;
    }

    // 후행 DDL — drop이면 DROP IF EXISTS 선행 후, 의존성 순서는 재시도로 흡수.
    apply_post(conn, &post, drop).await?;

    Ok(inserted)
}

/// 누적 튜플을 다행 INSERT로 적재하고 batch를 비운다. 반환은 적재 행 수.
async fn flush_insert(
    conn: &mut Conn,
    quoted: &str,
    col_list: &str,
    batch: &mut Vec<String>,
    ns: &str,
) -> Result<u64> {
    if batch.is_empty() {
        return Ok(0);
    }
    let n = batch.len() as u64;
    let sql = format!("INSERT INTO {quoted}{col_list} VALUES {}", batch.join(", "));
    conn.query_drop(sql)
        .await
        .map_err(|e| XBackupError::Failure(format!("{ns} INSERT 실패: {e}")))?;
    batch.clear();
    Ok(n)
}

/// 후행 DDL 적용 — drop이면 DROP IF EXISTS 선행, 이후 의존성 순서 무관 재시도(뷰↔뷰 등).
async fn apply_post(conn: &mut Conn, post: &[PostObj], drop: bool) -> Result<()> {
    if drop {
        for obj in post {
            if let Some(kw) = drop_keyword(&obj.kind) {
                if !obj.name.is_empty() {
                    let _ = conn
                        .query_drop(format!("DROP {kw} IF EXISTS {}", quote_ident(&obj.name)))
                        .await;
                }
            }
        }
    }
    let mut pending: Vec<&PostObj> = post.iter().collect();
    while !pending.is_empty() {
        let mut still: Vec<&PostObj> = Vec::new();
        let mut last_err: Option<String> = None;
        for obj in &pending {
            match run_ignore_exists(conn, &obj.sql).await {
                Ok(()) => {}
                Err(e) => {
                    last_err = Some(format!("{e}\n  SQL: {}", obj.sql));
                    still.push(obj);
                }
            }
        }
        if still.len() == pending.len() {
            return Err(XBackupError::Failure(format!(
                "후행 DDL(뷰/트리거/루틴/이벤트) 적용 실패(의존성 해소 불가): {}",
                last_err.unwrap_or_default()
            )));
        }
        pending = still;
    }
    Ok(())
}

/// 객체 kind → DROP 키워드.
fn drop_keyword(kind: &str) -> Option<&'static str> {
    match kind {
        "view" => Some("VIEW"),
        "trigger" => Some("TRIGGER"),
        "procedure" => Some("PROCEDURE"),
        "function" => Some("FUNCTION"),
        "event" => Some("EVENT"),
        _ => None,
    }
}

/// "이미 존재"(객체 중복) 오류는 무시하고 실행한다(drop=false 재적용·idempotent 경로).
/// 데이터 무결성 오류(중복 키 등)는 절대 삼키지 않는다.
async fn run_ignore_exists(conn: &mut Conn, sql: &str) -> Result<()> {
    match conn.query_drop(sql).await {
        Ok(()) => Ok(()),
        Err(e) => {
            if is_already_exists(&e) {
                tracing::debug!("이미 존재 — 건너뜀: {e}");
                Ok(())
            } else {
                Err(XBackupError::Failure(format!("{e}")))
            }
        }
    }
}

/// "이미 존재" 계열 서버 오류 코드인지 — 1050(table/view), 1304(routine), 1359(trigger), 1537(event).
fn is_already_exists(e: &mysql_async::Error) -> bool {
    if let mysql_async::Error::Server(se) = e {
        matches!(se.code, 1050 | 1304 | 1359 | 1537)
    } else {
        false
    }
}

/// 백업 소스와 복구 대상의 메이저 버전이 다르면 경고한다(차단하지 않음).
async fn warn_on_major_mismatch(conn: &mut Conn, source_version: &str) {
    let target: Option<String> = conn.query_first("SELECT VERSION()").await.ok().flatten();
    let Some(target) = target else { return };
    let major = |v: &str| {
        v.trim()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
    };
    let (sm, tm) = (major(source_version), major(&target));
    if !sm.is_empty() && !tm.is_empty() && sm != tm {
        tracing::warn!(
            "MySQL 메이저 버전 불일치 — 백업 소스={source_version}, 복구 대상={target}. \
             타입·기본값 차이를 복구 후 확인하세요"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drop_keyword_maps() {
        assert_eq!(drop_keyword("view"), Some("VIEW"));
        assert_eq!(drop_keyword("trigger"), Some("TRIGGER"));
        assert_eq!(drop_keyword("procedure"), Some("PROCEDURE"));
        assert_eq!(drop_keyword("function"), Some("FUNCTION"));
        assert_eq!(drop_keyword("event"), Some("EVENT"));
        assert_eq!(drop_keyword("unknown"), None);
    }
}
