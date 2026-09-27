//! PostgreSQL status 점검 — 연결·버전·DB 크기·테이블/행 수를 SQL로 조회한다(읽기 전용).
//!
//! Mongo status와 동일한 [`StatusReport`]/[`CheckItem`] 모델을 재사용하므로 `status`·`status
//! --all` 비교 뷰가 그대로 동작한다(키가 겹치는 항목은 source/target diff로 비교된다).

use super::conn::PgClient;
use crate::config::secret::Secret;
use crate::engine::mongo::status::{human_bytes, CheckItem, StatusReport};

const SYSTEM_SCHEMAS: &str = "'pg_catalog','information_schema','pg_toast'";

/// PostgreSQL 프로파일의 status 보고서를 만든다(연결 실패도 보고서로 표현).
pub async fn full_report(
    profile: &str,
    uri: &Secret,
    timeout_secs: Option<u64>,
    lang: crate::i18n::Lang,
) -> StatusReport {
    let pg = match PgClient::connect(uri, timeout_secs).await {
        Ok(p) => p,
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
            )
        }
    };
    let c = pg.client();
    let mut items = vec![CheckItem::ok(
        "connection",
        "connection",
        lang.sel("connected (PostgreSQL)", "연결 성공(PostgreSQL)"),
    )
    .with_value("OK")];

    // 현재 데이터베이스. 질의 실패는 **숨기지 않고 warn**으로 표면화한다(권한 부족이 거짓
    // 초록으로 보이지 않게 — FR-8 신호등).
    match c.query_one("SELECT current_database()", &[]).await {
        Ok(row) => {
            let db: String = row.get(0);
            items.push(
                CheckItem::ok(
                    "database",
                    "database",
                    lang.sel(&format!("current DB: {db}"), &format!("현재 DB: {db}")),
                )
                .with_value(db),
            );
        }
        Err(e) => items.push(CheckItem::warn(
            "database",
            "database",
            lang.sel(
                &format!("current_database query failed (privilege?): {e}"),
                &format!("current_database 조회 실패(권한 가능): {e}"),
            ),
        )),
    }

    // 버전 + 호환 판정(FR-8 #3). 매우 구버전(major<12)은 복구 호환 주의로 warn.
    match c.query_one("SHOW server_version", &[]).await {
        Ok(row) => {
            let v: String = row.get(0);
            let item = match parse_major(&v) {
                Some(m) if m < 12 => CheckItem::warn(
                    "version",
                    "version",
                    lang.sel(
                        &format!(
                            "PostgreSQL {v} — old version (<12), recovery compatibility caution"
                        ),
                        &format!("PostgreSQL {v} — 구버전(<12), 복구 호환에 주의"),
                    ),
                )
                .with_value(v),
                _ => CheckItem::ok(
                    "version",
                    "version",
                    lang.sel(&format!("PostgreSQL {v}"), &format!("PostgreSQL {v}")),
                )
                .with_value(v),
            };
            items.push(item);
        }
        Err(e) => items.push(CheckItem::warn(
            "version",
            "version",
            lang.sel(
                &format!("server_version query failed: {e}"),
                &format!("server_version 조회 실패: {e}"),
            ),
        )),
    }

    // 권한(FR-8 #2) — 백업은 전 사용자 테이블 read가 필요. 일부라도 SELECT가 없으면 warn.
    let perm_sql = format!(
        "SELECT count(*) FILTER (WHERE NOT has_table_privilege(c.oid, 'SELECT'))::bigint AS missing, \
                count(*)::bigint AS total \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname NOT IN ({SYSTEM_SCHEMAS})"
    );
    match c.query_one(perm_sql.as_str(), &[]).await {
        Ok(row) => {
            let missing: i64 = row.get("missing");
            let total: i64 = row.get("total");
            if missing > 0 {
                items.push(
                    CheckItem::warn(
                        "permission",
                        "privileges",
                        lang.sel(
                            &format!(
                                "no SELECT on {missing}/{total} user tables — backup may partially fail"
                            ),
                            &format!(
                                "사용자 테이블 {missing}/{total}개에 SELECT 권한 없음 — 백업이 일부 실패할 수 있음"
                            ),
                        ),
                    )
                    .with_value(format!("{missing}/{total} missing")),
                );
            } else {
                items.push(
                    CheckItem::ok(
                        "permission",
                        "privileges",
                        lang.sel(
                            &format!("SELECT on all user tables ({total})"),
                            &format!("전 사용자 테이블 SELECT 가능({total}개)"),
                        ),
                    )
                    .with_value(lang.sel("sufficient", "충분")),
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

    // 토폴로지/복제 역할 + read-only(FR-8 #4/#6). standby는 백업 소스로 적합(읽기 전용).
    match c
        .query_one(
            "SELECT pg_is_in_recovery() AS in_recovery, \
                    current_setting('default_transaction_read_only') AS ro",
            &[],
        )
        .await
    {
        Ok(row) => {
            let in_recovery: bool = row.get("in_recovery");
            let ro: String = row.get("ro");
            if in_recovery {
                items.push(
                    CheckItem::ok(
                        "topology",
                        "topology",
                        lang.sel(
                            "standby (replica) — read-only. suitable as backup source",
                            "standby(복제본) — 읽기 전용. 백업 소스로 적합",
                        ),
                    )
                    .with_value("standby"),
                );
                // standby 재생 지연(초). 재생 이력이 없으면 0.
                if let Ok(r) = c
                    .query_one(
                        "SELECT coalesce(extract(epoch FROM (now() - pg_last_xact_replay_timestamp())), 0)::bigint",
                        &[],
                    )
                    .await
                {
                    let lag: i64 = r.get(0);
                    items.push(
                        CheckItem::ok(
                            "replication_lag",
                            "replication lag",
                            lang.sel(
                                &format!("replay lag approx {lag}s"),
                                &format!("재생 지연 약 {lag}s"),
                            ),
                        )
                        .with_value(format!("{lag}s")),
                    );
                }
            } else {
                let note = if ro == "on" {
                    "primary(default_transaction_read_only=on)"
                } else {
                    "primary"
                };
                items.push(CheckItem::ok("topology", "topology", note).with_value("primary"));
                // 연결된 standby 수(권한 따라 0일 수 있음 — 정보성).
                if let Ok(r) = c
                    .query_one("SELECT count(*)::bigint FROM pg_stat_replication", &[])
                    .await
                {
                    let n: i64 = r.get(0);
                    items.push(
                        CheckItem::ok(
                            "replication",
                            "replication",
                            lang.sel(
                                &format!("{n} connected standby(s)"),
                                &format!("연결된 standby {n}개"),
                            ),
                        )
                        .with_value(n.to_string()),
                    );
                }
            }
        }
        Err(e) => items.push(CheckItem::warn(
            "topology",
            "topology",
            lang.sel(
                &format!("topology/replication check failed: {e}"),
                &format!("토폴로지/복제 점검 실패: {e}"),
            ),
        )),
    }

    // DB 크기.
    match c
        .query_one("SELECT pg_database_size(current_database())::bigint", &[])
        .await
    {
        Ok(row) => {
            let sz: i64 = row.get(0);
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
                &format!("DB size query failed (privilege?): {e}"),
                &format!("DB 크기 조회 실패(권한 가능): {e}"),
            ),
        )),
    }

    // 테이블 수.
    let count_sql = format!(
        "SELECT count(*)::bigint FROM pg_tables WHERE schemaname NOT IN ({SYSTEM_SCHEMAS})"
    );
    match c.query_one(count_sql.as_str(), &[]).await {
        Ok(row) => {
            let n: i64 = row.get(0);
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
                &format!("table count query failed (privilege?): {e}"),
                &format!("테이블 수 조회 실패(권한 가능): {e}"),
            ),
        )),
    }

    // 추정 행 수(reltuples — 전수 스캔 없이). ANALYZE 전이면 -1일 수 있어 GREATEST로 음수 차단(리뷰 #7).
    let rows_sql = format!(
        "SELECT greatest(0, coalesce(sum(c.reltuples), 0))::bigint \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname NOT IN ({SYSTEM_SCHEMAS})"
    );
    match c.query_one(rows_sql.as_str(), &[]).await {
        Ok(row) => {
            let n: i64 = row.get(0);
            items.push(
                CheckItem::ok(
                    "doc_count",
                    "rows",
                    lang.sel(
                        &format!("est. {n} rows (reltuples)"),
                        &format!("추정 {n}행(reltuples)"),
                    ),
                )
                .with_value(n.to_string()),
            );
        }
        Err(e) => items.push(CheckItem::warn(
            "doc_count",
            "rows",
            lang.sel(
                &format!("row count query failed (privilege?): {e}"),
                &format!("행 수 조회 실패(권한 가능): {e}"),
            ),
        )),
    }

    StatusReport::new(profile, items)
}

/// `server_version` 문자열에서 메이저 버전을 파싱한다(예: "16.2" → 16, "15beta1" → 15).
fn parse_major(v: &str) -> Option<u32> {
    let head: String = v
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    head.parse().ok()
}

/// 백업 전 PG 사전 점검(FR-8 차단 서브셋) — 연결 + 읽기 권한. 차단 결함이면 PrecheckFailed(exit 3).
///
/// 전체 [`full_report`](정보성, warn 위주)와 달리 백업을 *막는* 결함만 본다. backup 핸들러가
/// `mode.precheck && !--skip-precheck`일 때 호출해, 잘못 구성/권한 부족 대상에 대해 스트림
/// 도중 실패하는 대신 빠르게 exit 3로 미시작한다(엔진 무관 FR-8 / PRD §323).
pub async fn precheck(uri: &Secret, timeout_secs: Option<u64>) -> crate::error::Result<()> {
    use crate::error::XBackupError;
    let pg = PgClient::connect(uri, timeout_secs).await.map_err(|e| {
        XBackupError::PrecheckFailed(crate::tr!(
            "failed to connect to PostgreSQL: {e}",
            "PG 연결 실패: {e}"
        ))
    })?;
    let c = pg.client();
    let perm_sql = format!(
        "SELECT count(*) FILTER (WHERE NOT has_table_privilege(c.oid, 'SELECT'))::bigint AS missing, \
                count(*)::bigint AS total \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind = 'r' AND n.nspname NOT IN ({SYSTEM_SCHEMAS})"
    );
    let row = c.query_one(perm_sql.as_str(), &[]).await.map_err(|e| {
        XBackupError::PrecheckFailed(crate::tr!(
            "the privilege-check query failed: {e}",
            "권한 점검 질의 실패: {e}"
        ))
    })?;
    let missing: i64 = row.get("missing");
    let total: i64 = row.get("total");
    if missing > 0 {
        return Err(XBackupError::PrecheckFailed(crate::tr!("the backup user lacks SELECT on {missing}/{total} user table(s) — SELECT is required on everything being backed up (grant it and retry, or bypass with --skip-precheck).", "백업 사용자가 사용자 테이블 {missing}/{total}개에 SELECT 권한이 없습니다 — \
             백업 대상 전체에 SELECT가 필요합니다(권한 부여 후 재시도하거나 --skip-precheck로 우회).")));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_major;

    #[test]
    fn parse_major_handles_common_formats() {
        assert_eq!(parse_major("16.2"), Some(16));
        assert_eq!(parse_major("15.4 (Debian)"), Some(15));
        assert_eq!(parse_major(" 14"), Some(14));
        assert_eq!(parse_major("17beta1"), Some(17));
        assert_eq!(parse_major("garbage"), None);
    }
}
