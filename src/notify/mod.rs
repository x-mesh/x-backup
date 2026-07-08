//! 알림(P3-2) — generic JSON webhook POST.
//!
//! daemon(P3-1)이 백업 실행 결과를 알린다. reqwest는 update 경로로 이미 트리에 있어
//! **신규 의존 0**이다(rustls 체인). Slack/Discord 등은 generic webhook을 받으므로 별도
//! 어댑터 없이 동작한다(페이로드 포맷은 아래 [`BackupEvent`]).
//!
//! URL은 시크릿 취급(토큰 포함 가능) — config에는 **환경변수 이름**만 둔다
//! (`notify.webhook_url_env`, 시크릿 평문 비저장 원칙 FR-10과 동일).
//! 알림 실패는 백업 결과를 바꾸지 않는다 — 호출자가 경고 로그만 남긴다(best-effort).

use serde::Serialize;

use crate::error::{Result, XBackupError};

/// 전송 타임아웃(초) — 알림이 스케줄 루프를 오래 막지 않게 한다.
const SEND_TIMEOUT_SECS: u64 = 10;

/// 백업 실행 이벤트 페이로드(JSON body).
#[derive(Debug, Serialize)]
pub struct BackupEvent<'a> {
    /// 이벤트 종류(현재 "backup").
    pub event: &'a str,
    /// 프로파일 이름.
    pub profile: &'a str,
    /// 성공 여부(경고 동반 성공 exit 4는 ok=true + exit_code=4).
    pub ok: bool,
    /// 종료 코드 계약(0~5).
    pub exit_code: u8,
    /// 실패/경고 사유(성공이면 None).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 실행 소요(밀리초).
    pub duration_ms: u64,
    /// 발생 시각(UTC RFC3339).
    pub at: String,
}

/// webhook URL로 이벤트를 POST한다(JSON). 2xx 외 응답은 에러.
pub async fn send_webhook(url: &str, event: &BackupEvent<'_>) -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("x-backup/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(SEND_TIMEOUT_SECS))
        .build()
        .map_err(|e| XBackupError::Failure(format!("webhook 클라이언트 생성 실패: {e}")))?;
    let resp = client
        .post(url)
        .json(event)
        .send()
        .await
        .map_err(|e| XBackupError::Failure(format!("webhook 전송 실패: {e}")))?;
    if !resp.status().is_success() {
        return Err(XBackupError::Failure(format!(
            "webhook 응답 비정상: HTTP {}",
            resp.status()
        )));
    }
    Ok(())
}

/// config의 `webhook_url_env`(환경변수 이름)를 실제 URL로 해석한다. env 부재는 설정 오류.
pub fn resolve_webhook_url(env_name: &str) -> Result<String> {
    match std::env::var(env_name) {
        Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
        _ => Err(XBackupError::Config(format!(
            "notify.webhook_url_env가 가리키는 환경변수 '{env_name}'가 비어 있거나 없습니다"
        ))),
    }
}
