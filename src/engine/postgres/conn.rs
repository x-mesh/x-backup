//! PostgreSQL 연결 헬퍼 — `tokio-postgres`로 연결하고 백그라운드 연결 task를 띄운다.
//!
//! **TLS(리뷰 #3):** rustls(ring + webpki 신뢰 루트) 커넥터를 항상 제공한다. URI의
//! `sslmode`가 드라이버 동작을 정한다 — 기본 `prefer`면 TLS를 시도하고 서버가 미지원이면
//! 평문으로 폴백(로컬 컨테이너 호환), `require`/`verify-ca`/`verify-full`이면 TLS를 강제한다.
//! 연결 타임아웃은 Mongo와 동일하게 기본 5초.

use std::sync::Arc;
use std::time::Duration;

use tokio_postgres::Client;
use tokio_postgres_rustls::MakeRustlsConnect;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// 설정·URI에 타임아웃이 없을 때의 기본 연결 타임아웃(초) — Mongo와 일관.
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// PostgreSQL 클라이언트 래퍼(드라이버 `Client` 보유 — status/backup/restore 공용).
pub struct PgClient {
    client: Client,
}

impl PgClient {
    /// URI 시크릿으로 연결한다(rustls TLS 협상). 연결 task는 백그라운드로 spawn해 구동 유지.
    pub async fn connect(uri: &Secret, timeout_secs: Option<u64>) -> Result<Self> {
        let dur = Duration::from_secs(timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let tls = make_tls();
        let fut = tokio_postgres::connect(uri.expose(), tls);
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

/// rustls 기반 TLS 커넥터 — ring provider + webpki 신뢰 루트(서버 인증서 검증).
fn make_tls() -> MakeRustlsConnect {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("rustls 기본 프로토콜 버전 구성")
    .with_root_certificates(roots)
    .with_no_client_auth();
    MakeRustlsConnect::new(config)
}
