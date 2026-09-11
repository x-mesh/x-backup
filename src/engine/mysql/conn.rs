//! MySQL/MariaDB 연결 헬퍼 — `mysql_async`로 연결한다.
//!
//! **스킴:** `mysql_async`는 `mysql://`만 인식하므로 `mariadb://`는 `mysql://`로 정규화한다.
//!
//! **TLS:** `mysql_async`는 PostgreSQL의 `sslmode=prefer`(TLS 시도 후 평문 폴백)에 해당하는
//! 자동 폴백이 없다. URI 쿼리의 커스텀 `ssl-mode` 파라미터로 명시 제어한다 — 기본(미지정)은
//! 평문(로컬 컨테이너 호환), `REQUIRED`/`PREFERRED`는 검증 없이 암호화, `VERIFY_CA`/
//! `VERIFY_IDENTITY`는 인증서(및 호스트) 검증. `mysql_async`의 `from_url`은 미지원 쿼리
//! 파라미터를 거부하므로 `ssl-mode`는 파싱 전에 분리한다. 연결 타임아웃은 Mongo/PG와 동일 기본 5초.

use std::time::Duration;

use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 설정·URI에 타임아웃이 없을 때의 기본 연결 타임아웃(초) — Mongo/PG와 일관.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// MySQL 클라이언트 래퍼(드라이버 `Conn` 보유 — status/backup/restore 공용).
///
/// `mysql_async`의 쿼리 API는 `&mut Conn`(또는 `&mut Transaction`)을 요구하므로 PostgreSQL의
/// 공유 `&Client`와 달리 가변 참조로 다룬다.
pub struct MysqlClient {
    conn: Conn,
}

impl MysqlClient {
    /// URI 시크릿으로 연결한다. 연결 타임아웃을 적용한다.
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let dur = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let opts = build_opts(uri.expose())?;
        let conn = tokio::time::timeout(dur, Conn::new(opts))
            .await
            .map_err(|_| {
                XBackupError::Failure(crate::tr!(
                    "MySQL connection timed out ({}s)",
                    "MySQL 연결 타임아웃({}s)",
                    dur.as_secs()
                ))
            })?
            .map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "failed to connect to MySQL: {}",
                    "MySQL 연결 실패: {}",
                    describe(&e)
                ))
            })?;
        Ok(Self { conn })
    }

    /// 복구 대상에 연결한다. 대상 데이터베이스가 없고 `create_missing`이 참이면 생성 후 연결한다.
    ///
    /// `create_missing`이 거짓이면 데이터베이스가 없을 때 `None`을 반환한다. `--dry-run`은 이
    /// 경로로 상태를 바꾸지 않고 빈 대상으로 계획할 수 있다. 데이터베이스 생성 권한이 없으면
    /// 원래 서버 오류를 포함한 명확한 실패를 반환한다.
    pub async fn connect_restore_target(
        uri: &Secret,
        timeout_secs: Option<u64>,
        create_missing: bool,
    ) -> Result<Option<Self>> {
        let dur = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let opts = build_opts(uri.expose())?;
        let db_name = opts
            .db_name()
            .filter(|name| !name.is_empty())
            .map(str::to_string)
            .ok_or_else(|| {
                XBackupError::Usage(crate::tr!(
                    "the MySQL restore target URI must name a database (mysql://.../<db>)",
                    "복구 대상 MySQL URI에 데이터베이스가 필요합니다(mysql://.../<db>)"
                ))
            })?;

        match tokio::time::timeout(dur, Conn::new(opts.clone())).await {
            Ok(Ok(conn)) => return Ok(Some(Self { conn })),
            Ok(Err(err)) if is_unknown_database(&err) => {}
            Ok(Err(err)) => {
                return Err(XBackupError::Failure(crate::tr!(
                    "failed to connect to MySQL: {}",
                    "MySQL 연결 실패: {}",
                    describe(&err)
                )))
            }
            Err(_) => {
                return Err(XBackupError::Failure(crate::tr!(
                    "MySQL connection timed out ({}s)",
                    "MySQL 연결 타임아웃({}s)",
                    dur.as_secs()
                )))
            }
        }

        if !create_missing {
            return Ok(None);
        }

        let server_opts: Opts = OptsBuilder::from_opts(opts.clone())
            .db_name(None::<String>)
            .into();
        let mut server = tokio::time::timeout(dur, Conn::new(server_opts))
            .await
            .map_err(|_| {
                XBackupError::Failure(crate::tr!("MySQL server connection timed out ({}s)", "MySQL 서버 연결 타임아웃({}s)", dur.as_secs()))
            })?
            .map_err(|e| {
                XBackupError::Failure(crate::tr!("failed to connect to the MySQL server (preparing to create the target database): {}", "MySQL 서버 연결 실패(대상 데이터베이스 생성 준비): {}", describe(&e)))
            })?;

        let quoted = super::util::quote_ident(&db_name);
        server
            .query_drop(format!("CREATE DATABASE IF NOT EXISTS {quoted}"))
            .await
            .map_err(|e| {
                XBackupError::Failure(crate::tr!("failed to create MySQL target database '{db_name}': {}. This account needs the CREATE DATABASE privilege", "MySQL 대상 데이터베이스 '{db_name}' 생성 실패: {}. 이 계정에는 CREATE DATABASE 권한이 필요합니다", describe(&e)))
            })?;
        let _ = server.disconnect().await;

        let conn = tokio::time::timeout(dur, Conn::new(opts))
            .await
            .map_err(|_| {
                XBackupError::Failure(crate::tr!(
                    "connection to the newly created MySQL database '{db_name}' timed out ({}s)",
                    "생성한 MySQL 데이터베이스 '{db_name}' 연결 타임아웃({}s)",
                    dur.as_secs()
                ))
            })?
            .map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "failed to connect to the newly created MySQL database '{db_name}': {}",
                    "생성한 MySQL 데이터베이스 '{db_name}' 연결 실패: {}",
                    describe(&e)
                ))
            })?;
        tracing::info!(database = %db_name, "MySQL 복구 대상 데이터베이스 생성 완료");
        Ok(Some(Self { conn }))
    }

    /// 내부 드라이버 연결의 가변 참조(쿼리 실행용).
    pub fn conn_mut(&mut self) -> &mut Conn {
        &mut self.conn
    }

    /// 소유 `Conn`을 반환한다 — binlog 스트림(`get_binlog_stream`)이 연결을 소비할 때 사용.
    pub fn into_conn(self) -> Conn {
        self.conn
    }

    /// 서버 버전 문자열(`SELECT VERSION()`). MariaDB는 `10.x.y-MariaDB` 형태로 식별 가능.
    pub async fn server_version(&mut self) -> Result<Option<String>> {
        self.conn
            .query_first::<String, _>("SELECT VERSION()")
            .await
            .map_err(|e| {
                XBackupError::Failure(crate::tr!(
                    "failed to query the MySQL version: {}",
                    "MySQL 버전 조회 실패: {}",
                    describe(&e)
                ))
            })
    }
}

/// URI를 `mysql_async` `Opts`로 변환한다 — `mariadb://` 정규화 + 커스텀 `ssl-mode` 처리.
fn build_opts(uri: &str) -> Result<Opts> {
    let normalized = match uri.strip_prefix("mariadb://") {
        Some(rest) => format!("mysql://{rest}"),
        None => uri.to_string(),
    };
    let (clean_url, ssl_mode) = extract_ssl_mode(&normalized);
    let base = Opts::from_url(&clean_url).map_err(|e| {
        XBackupError::Config(crate::tr!(
            "failed to parse the MySQL URI: {e}",
            "MySQL URI 파싱 실패: {e}"
        ))
    })?;
    match ssl_opts_for(ssl_mode.as_deref()) {
        Some(ssl) => Ok(OptsBuilder::from_opts(base).ssl_opts(ssl).into()),
        None => Ok(base),
    }
}

/// URI 쿼리에서 `ssl-mode`(별칭 `sslmode`/`ssl_mode`)를 떼어내고 나머지 쿼리만 남긴 URL과
/// 추출한 ssl-mode 값을 돌려준다. `mysql_async`의 `from_url`은 미지원 파라미터를 거부하므로
/// 호출 전에 반드시 제거해야 한다.
fn extract_ssl_mode(url: &str) -> (String, Option<String>) {
    let Some((base, query)) = url.split_once('?') else {
        return (url.to_string(), None);
    };
    let mut kept: Vec<&str> = Vec::new();
    let mut ssl_mode = None;
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let key = pair.split('=').next().unwrap_or("");
        match key.to_ascii_lowercase().as_str() {
            "ssl-mode" | "sslmode" | "ssl_mode" => {
                ssl_mode = pair.split_once('=').map(|(_, v)| v.to_string());
            }
            _ => kept.push(pair),
        }
    }
    let rebuilt = if kept.is_empty() {
        base.to_string()
    } else {
        format!("{base}?{}", kept.join("&"))
    };
    (rebuilt, ssl_mode)
}

/// ssl-mode 문자열 → `SslOpts`. 미지정/`DISABLED`는 평문(None).
fn ssl_opts_for(mode: Option<&str>) -> Option<SslOpts> {
    let mode = mode?.trim().to_ascii_uppercase();
    match mode.as_str() {
        "" | "DISABLED" => None,
        "VERIFY_IDENTITY" => Some(SslOpts::default()),
        "VERIFY_CA" => Some(SslOpts::default().with_danger_skip_domain_validation(true)),
        // REQUIRED/PREFERRED(및 미인식 값) — 암호화는 하되 인증서 검증은 생략(로컬/자체서명 호환).
        _ => Some(
            SslOpts::default()
                .with_danger_accept_invalid_certs(true)
                .with_danger_skip_domain_validation(true),
        ),
    }
}

/// `mysql_async::Error`의 사유를 한 줄로 합친다(source 체인이 있으면 펼친다).
fn describe(e: &mysql_async::Error) -> String {
    use std::error::Error;
    let mut msg = e.to_string();
    let mut src = e.source();
    while let Some(s) = src {
        let detail = s.to_string();
        if !detail.is_empty() && !msg.contains(&detail) {
            msg.push_str(": ");
            msg.push_str(&detail);
        }
        src = s.source();
    }
    msg
}

/// MySQL ER_BAD_DB_ERROR. 다른 인증·네트워크 오류에는 자동 생성을 시도하지 않는다.
fn is_unknown_database(error: &mysql_async::Error) -> bool {
    matches!(error, mysql_async::Error::Server(server) if server.code == 1049)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_ssl_mode_none() {
        let (url, mode) = extract_ssl_mode("mysql://u:p@h:3306/db");
        assert_eq!(url, "mysql://u:p@h:3306/db");
        assert_eq!(mode, None);
    }

    #[test]
    fn extract_ssl_mode_strips_param() {
        let (url, mode) = extract_ssl_mode("mysql://h/db?ssl-mode=REQUIRED&prefer_socket=false");
        assert_eq!(url, "mysql://h/db?prefer_socket=false");
        assert_eq!(mode.as_deref(), Some("REQUIRED"));
    }

    #[test]
    fn extract_ssl_mode_only_param() {
        let (url, mode) = extract_ssl_mode("mysql://h/db?sslmode=VERIFY_CA");
        assert_eq!(url, "mysql://h/db");
        assert_eq!(mode.as_deref(), Some("VERIFY_CA"));
    }

    #[test]
    fn ssl_opts_mapping() {
        assert!(ssl_opts_for(None).is_none());
        assert!(ssl_opts_for(Some("DISABLED")).is_none());
        assert!(ssl_opts_for(Some("REQUIRED")).is_some());
        assert!(ssl_opts_for(Some("VERIFY_IDENTITY")).is_some());
    }

    #[test]
    fn detects_only_unknown_database_error() {
        let unknown = mysql_async::Error::Server(mysql_async::ServerError {
            code: 1049,
            message: "Unknown database 'missing'".into(),
            state: "42000".into(),
        });
        let denied = mysql_async::Error::Server(mysql_async::ServerError {
            code: 1045,
            message: "Access denied".into(),
            state: "28000".into(),
        });
        assert!(is_unknown_database(&unknown));
        assert!(!is_unknown_database(&denied));
    }
}
