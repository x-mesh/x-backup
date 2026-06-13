//! MongoDB 연결 옵션 공통 구성 — server-selection/connect 타임아웃 정책.
//!
//! 세 connect 경로([`super::meta::MongoMeta`]·[`super::oplog::OplogReader`]·
//! [`super::status::StatusChecker`])가 동일하게 쓴다. URI를 파싱한 뒤:
//! - URI에 `serverSelectionTimeoutMS`가 있으면 **URI가 우선**한다. config 타임아웃이
//!   설정돼 있고 그 값과 다르면 경고를 남긴다(설정한 경우에만 — 기본값에는 경고 없음).
//! - URI에 없으면 config 타임아웃(없으면 [`DEFAULT_TIMEOUT_SECS`])을 적용한다.
//!
//! 적용 대상은 server-selection(최초 접속에서 체감하는 타임아웃)과 connect(서버별 TCP)
//! 둘 다다 — URI가 connect만 따로 지정하지 않은 한 같은 값으로 맞춘다.

use std::time::Duration;

use mongodb::options::ClientOptions;

use crate::config::secret::Secret;
use crate::error::{Result, XBackupError};

/// URI·config 모두 타임아웃을 지정하지 않을 때의 기본값(초).
pub const DEFAULT_TIMEOUT_SECS: u64 = 5;

/// URI를 파싱해 타임아웃 정책을 적용한 [`ClientOptions`]를 만든다.
///
/// `config_timeout_secs`는 프로파일의 `source.connect_timeout_secs`다(`None`이면 미설정).
pub async fn client_options(
    uri: &Secret,
    config_timeout_secs: Option<u64>,
) -> Result<ClientOptions> {
    let mut options = ClientOptions::parse(uri.expose())
        .await
        .map_err(|e| XBackupError::Failure(format!("MongoDB URI 파싱 실패: {e}")))?;

    let effective = Duration::from_secs(config_timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

    match options.server_selection_timeout {
        // URI가 명시 → URI 우선. config가 설정돼 있고 다르면 경고(기본값에는 경고 안 함).
        Some(uri_timeout) => {
            if let Some(cfg) = config_timeout_secs {
                if uri_timeout != Duration::from_secs(cfg) {
                    tracing::warn!(
                        "URI의 serverSelectionTimeoutMS({}ms)가 설정 타임아웃({}s)과 다릅니다 \
                         — URI 값을 사용합니다",
                        uri_timeout.as_millis(),
                        cfg
                    );
                }
            }
            tracing::debug!(
                server_selection_ms = uri_timeout.as_millis() as u64,
                "MongoDB 타임아웃: URI 지정값 사용"
            );
        }
        // URI 미지정 → config(또는 기본 5초) 적용.
        None => {
            options.server_selection_timeout = Some(effective);
            if options.connect_timeout.is_none() {
                options.connect_timeout = Some(effective);
            }
            tracing::debug!(
                server_selection_ms = effective.as_millis() as u64,
                source = if config_timeout_secs.is_some() {
                    "config"
                } else {
                    "default"
                },
                "MongoDB 타임아웃 적용"
            );
        }
    }

    Ok(options)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn applies_default_when_uri_silent() {
        let uri = Secret::new("mongodb://localhost:27017/".to_string());
        let opts = client_options(&uri, None).await.unwrap();
        assert_eq!(
            opts.server_selection_timeout,
            Some(Duration::from_secs(DEFAULT_TIMEOUT_SECS))
        );
    }

    #[tokio::test]
    async fn applies_config_when_uri_silent() {
        let uri = Secret::new("mongodb://localhost:27017/".to_string());
        let opts = client_options(&uri, Some(12)).await.unwrap();
        assert_eq!(opts.server_selection_timeout, Some(Duration::from_secs(12)));
    }

    #[tokio::test]
    async fn uri_wins_over_config() {
        let uri =
            Secret::new("mongodb://localhost:27017/?serverSelectionTimeoutMS=2000".to_string());
        // config가 5초여도 URI(2초)가 우선한다.
        let opts = client_options(&uri, Some(5)).await.unwrap();
        assert_eq!(
            opts.server_selection_timeout,
            Some(Duration::from_millis(2000))
        );
    }
}
