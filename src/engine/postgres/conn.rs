//! PostgreSQL 연결 헬퍼 — `tokio-postgres`로 연결하고 백그라운드 연결 task를 띄운다.
//!
//! 1차는 로컬·내부망 가정으로 **NoTls**다(TLS는 후속 — `sslmode`/rustls 배선). 연결 타임아웃은
//! Mongo와 동일하게 기본 5초이며, URI에 별도 타임아웃 파라미터는 두지 않는다.

use std::time::Duration;

use tokio_postgres::{Client, NoTls};

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 설정·URI에 타임아웃이 없을 때의 기본 연결 타임아웃(초) — Mongo와 일관.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// PostgreSQL 클라이언트 래퍼(드라이버 `Client` 보유 — status/backup/restore 공용).
pub struct PgClient {
    client: Client,
}

impl PgClient {
    /// URI 시크릿으로 연결한다(NoTls). 연결 task는 백그라운드로 spawn해 구동을 유지한다.
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let dur = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let fut = tokio_postgres::connect(uri.expose(), NoTls);
        let (client, connection) = tokio::time::timeout(dur, fut)
            .await
            .map_err(|_| {
                XBackupError::Failure(format!("PostgreSQL 연결 타임아웃({}s)", dur.as_secs()))
            })?
            .map_err(|e| XBackupError::Failure(format!("PostgreSQL 연결 실패: {e}")))?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::debug!("PostgreSQL 연결 종료: {e}");
            }
        });
        Ok(Self { client })
    }

    /// 내부 드라이버 클라이언트 참조.
    pub fn client(&self) -> &Client {
        &self.client
    }
}
