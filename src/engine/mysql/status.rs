//! MySQL status 점검 — 연결·버전·DB 크기·테이블/행 수를 SQL로 조회한다(읽기 전용).
//!
//! Mongo/PG status와 동일한 [`StatusReport`]/[`CheckItem`] 모델·키를 재사용하므로
//! `status`·`status --all` 비교 뷰가 그대로 동작한다.

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Row};

use super::conn::MysqlClient;
use crate::config::secret::Secret;
use crate::engine::mongo::status::{human_bytes, CheckItem, StatusReport};

/// MySQL 프로파일의 status 보고서를 만든다(연결 실패도 보고서로 표현).
pub async fn full_report(
    profile: &str,
    uri: &Secret,
    timeout_secs: Option<u64>,
    lang: crate::i18n::Lang,
) -> StatusReport {
    let mut client = match MysqlClient::connect(uri, timeout_secs).await {
        Ok(c) => c,
        Err(e) => {
            return StatusReport::new(
                profile,
                vec![CheckItem::fail(
                    "connection",
                    "connection",
                    lang.sel(
                        &format!("connection failed: {e}"),
                        &format!("연결 실패: {e}"),
                    ),
                )
                .with_value(lang.sel("connection failed", "연결 실패"))],
            );
        }
    };
    let conn = client.conn_mut();
    let mut items = vec![CheckItem::ok(
        "connection",
        "connection",
        lang.sel("connected (MySQL)", "연결 성공(MySQL)"),
    )
    .with_value("OK")];

    // 현재 데이터베이스.
    match conn
        .query_first::<Option<String>, _>("SELECT DATABASE()")
        .await
    {
        Ok(db) => {
            let db = db.flatten().unwrap_or_default();
            if db.is_empty() {
                items.push(CheckItem::warn(
                    "database",
                    "database",
                    lang.sel(
                        "no database in URI — backup requires mysql://.../<db>",
                        "URI에 데이터베이스 없음 — 백업은 mysql://.../<db> 필요",
                    ),
                ));
            } else {
                items.push(
                    CheckItem::ok(
                        "database",
                        "database",
                        lang.sel(&format!("current DB: {db}"), &format!("현재 DB: {db}")),
                    )
                    .with_value(db),
                );
            }
        }
        Err(e) => items.push(CheckItem::warn(
            "database",
            "database",
            lang.sel(
                &format!("DATABASE() query failed: {e}"),
                &format!("DATABASE() 조회 실패: {e}"),
            ),
        )),
    }

    // 버전 + 호환 판정. MySQL 5.x는 복구 호환 주의로 warn(MariaDB 10/11은 정상).
    match conn.query_first::<String, _>("SELECT VERSION()").await {
        Ok(Some(v)) => {
            let is_maria = v.to_ascii_lowercase().contains("mariadb");
            let item = match parse_major(&v) {
                Some(m) if !is_maria && m < 8 => CheckItem::warn(
                    "version",
                    "version",
                    lang.sel(
                        &format!("MySQL {v} — old version (<8.0), recovery compatibility caution"),
                        &format!("MySQL {v} — 구버전(<8.0), 복구 호환에 주의"),
                    ),
                )
                .with_value(v.clone()),
                _ => {
                    let label = if is_maria { "MariaDB" } else { "MySQL" };
                    CheckItem::ok("version", "version", format!("{label} {v}"))
                        .with_value(v.clone())
                }
            };
            items.push(item);
        }
        Ok(None) => items.push(CheckItem::warn(
            "version",
            "version",
            lang.sel("VERSION() returned empty", "VERSION() 결과 없음"),
        )),
        Err(e) => items.push(CheckItem::warn(
            "version",
            "version",
            lang.sel(
                &format!("VERSION() query failed: {e}"),
                &format!("VERSION() 조회 실패: {e}"),
            ),
        )),
    }

    // 권한 — 백업은 사용자 테이블 read가 필요. SHOW GRANTS에서 SELECT/ALL 존재 여부 best-effort 확인.
    match conn
        .query::<String, _>("SHOW GRANTS FOR CURRENT_USER()")
        .await
    {
        Ok(grants) => {
            let has_select = grants.iter().any(|g| {
                let u = g.to_ascii_uppercase();
                u.contains("ALL PRIVILEGES") || u.contains("SELECT")
            });
            if has_select {
                items.push(
                    CheckItem::ok(
                        "permission",
                        "privileges",
                        lang.sel("SELECT granted", "SELECT 권한 있음"),
                    )
                    .with_value(lang.sel("sufficient", "충분")),
                );
            } else {
                items.push(
                    CheckItem::warn(
                        "permission",
                        "privileges",
                        lang.sel(
                            "no SELECT grant detected — backup may fail",
                            "SELECT 권한 미확인 — 백업이 실패할 수 있음",
                        ),
                    )
                    .with_value(lang.sel("uncertain", "불확실")),
                );
            }
        }
        Err(e) => items.push(CheckItem::warn(
            "permission",
            "privileges",
            lang.sel(
                &format!("privilege check failed: {e}"),
                &format!("권한 점검 실패: {e}"),
            ),
        )),
    }

    // 토폴로지/복제 역할 + read-only. replica(SHOW REPLICA/SLAVE STATUS 행 존재)는 백업 소스로 적합.
    push_topology(conn, &mut items, lang).await;

    // DB 크기.
    match conn
        .query_first::<u64, _>(
            "SELECT COALESCE(SUM(data_length + index_length), 0) \
             FROM information_schema.TABLES WHERE TABLE_SCHEMA = DATABASE()",
        )
        .await
    {
        Ok(sz) => {
            let sz = sz.unwrap_or(0) as i64;
            items.push(
                CheckItem::ok(
                    "estimated_size",
                    "est. size",
                    lang.sel(
                        &format!("DB size {}", human_bytes(sz)),
                        &format!("DB 크기 {}", human_bytes(sz)),
                    ),
                )
                .with_value(human_bytes(sz)),
            );
        }
        Err(e) => items.push(CheckItem::warn(
            "estimated_size",
            "est. size",
            lang.sel(
                &format!("DB size query failed: {e}"),
                &format!("DB 크기 조회 실패: {e}"),
            ),
        )),
    }

    // 테이블 수.
    match conn
        .query_first::<i64, _>(
            "SELECT COUNT(*) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'",
        )
        .await
    {
        Ok(n) => {
            let n = n.unwrap_or(0);
            items.push(
                CheckItem::ok(
                    "collection_count",
                    "tables",
                    lang.sel(&format!("{n} user tables"), &format!("사용자 테이블 {n}개")),
                )
                .with_value(n.to_string()),
            );
        }
        Err(e) => items.push(CheckItem::warn(
            "collection_count",
            "tables",
            lang.sel(
                &format!("table count query failed: {e}"),
                &format!("테이블 수 조회 실패: {e}"),
            ),
        )),
    }

    // 추정 행 수(TABLE_ROWS — 전수 스캔 없이). InnoDB는 추정치(오차 가능).
    match conn
        .query_first::<i64, _>(
            "SELECT COALESCE(SUM(TABLE_ROWS), 0) FROM information_schema.TABLES \
             WHERE TABLE_SCHEMA = DATABASE() AND TABLE_TYPE = 'BASE TABLE'",
        )
        .await
    {
        Ok(n) => {
            let n = n.unwrap_or(0);
            items.push(
                CheckItem::ok(
                    "doc_count",
                    "rows",
                    lang.sel(
                        &format!("est. {n} rows (information_schema)"),
                        &format!("추정 {n}행(information_schema)"),
                    ),
                )
                .with_value(n.to_string()),
            );
        }
        Err(e) => items.push(CheckItem::warn(
            "doc_count",
            "rows",
            lang.sel(
                &format!("row count query failed: {e}"),
                &format!("행 수 조회 실패: {e}"),
            ),
        )),
    }

    StatusReport::new(profile, items)
}

/// 토폴로지/복제 역할 항목을 push한다(replica 여부 + 재생 지연).
async fn push_topology(conn: &mut Conn, items: &mut Vec<CheckItem>, lang: crate::i18n::Lang) {
    // SHOW REPLICA STATUS(8.0.22+) → 행 있으면 replica. 구버전/MariaDB는 SHOW SLAVE STATUS.
    let replica_row = match conn.query::<Row, _>("SHOW REPLICA STATUS").await {
        Ok(rows) => rows.into_iter().next(),
        Err(_) => conn
            .query::<Row, _>("SHOW SLAVE STATUS")
            .await
            .ok()
            .and_then(|r| r.into_iter().next()),
    };
    if let Some(row) = replica_row {
        items.push(
            CheckItem::ok(
                "topology",
                "topology",
                lang.sel(
                    "replica — read-friendly. suitable as backup source",
                    "replica(복제본) — 읽기 적합. 백업 소스로 적합",
                ),
            )
            .with_value("replica"),
        );
        let lag = row
            .get::<Option<u64>, _>("Seconds_Behind_Source")
            .or_else(|| row.get::<Option<u64>, _>("Seconds_Behind_Master"))
            .flatten();
        if let Some(lag) = lag {
            items.push(
                CheckItem::ok(
                    "replication_lag",
                    "replication lag",
                    lang.sel(&format!("replica lag {lag}s"), &format!("복제 지연 {lag}s")),
                )
                .with_value(format!("{lag}s")),
            );
        }
    } else {
        let ro: Option<u64> = conn.query_first("SELECT @@read_only").await.ok().flatten();
        let note = if ro == Some(1) {
            "primary (read_only=ON)"
        } else {
            "primary"
        };
        items.push(CheckItem::ok("topology", "topology", note).with_value("primary"));
    }
}

/// `VERSION()` 문자열에서 메이저 버전을 파싱한다(예: "8.0.39" → 8, "10.11.2-MariaDB" → 10).
fn parse_major(v: &str) -> Option<u32> {
    let head: String = v
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    head.parse().ok()
}

/// 백업 전 MySQL 사전 점검(FR-8 차단 서브셋) — 연결 + 대상 DB 지정. 차단 결함이면 PrecheckFailed(exit 3).
///
/// 전체 [`full_report`](정보성, warn 위주)와 달리 백업을 *막는* 결함만 본다. 테이블별 SELECT
/// 권한은 비용 때문에 사전 점검하지 않고 백업 스트림에서 fail-fast로 표면화한다.
pub async fn precheck(uri: &Secret, timeout_secs: Option<u64>) -> crate::error::Result<()> {
    use crate::error::XBackupError;
    let mut client = MysqlClient::connect(uri, timeout_secs).await.map_err(|e| {
        XBackupError::PrecheckFailed(crate::tr!(
            "failed to connect to MySQL: {e}",
            "MySQL 연결 실패: {e}"
        ))
    })?;
    let conn = client.conn_mut();
    let db: Option<String> = conn
        .query_first::<Option<String>, _>("SELECT DATABASE()")
        .await
        .map_err(|e| {
            XBackupError::PrecheckFailed(crate::tr!(
                "the DATABASE() query failed: {e}",
                "DATABASE() 질의 실패: {e}"
            ))
        })?
        .flatten();
    if db.as_deref().unwrap_or("").is_empty() {
        return Err(XBackupError::PrecheckFailed(crate::tr!(
            "the URI names no backup target database (mysql://user@host/<db> is required).",
            "백업 대상 데이터베이스가 URI에 없습니다(mysql://user@host/<db> 형식 필요)."
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_major;

    #[test]
    fn parse_major_handles_common_formats() {
        assert_eq!(parse_major("8.0.39"), Some(8));
        assert_eq!(parse_major("5.7.44-log"), Some(5));
        assert_eq!(parse_major("10.11.2-MariaDB-1:10.11"), Some(10));
        assert_eq!(parse_major("8.4.0"), Some(8));
        assert_eq!(parse_major("garbage"), None);
    }
}
