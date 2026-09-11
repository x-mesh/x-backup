//! 생명주기 훅(pre/post/on_error) — 백업/복구/prune의 정해진 지점에서 사용자 셸 명령을
//! 실행한다(PRD-04). 스케줄러를 내장하지 않는 headless 도구의 얇은 통합 표면이다.
//!
//! ## 게이팅 vs 관측
//! - `pre_*`(게이트): 비-0 종료면 작업을 **중단**한다([`HookSet::run_gate`] → [`XBackupError::Failure`]).
//! - `post_*`/`on_error`(관측): 실패해도 **경고 로그만** 남기고 작업 판정을 바꾸지 않는다
//!   ([`HookSet::run_observe`]).
//!
//! ## 시크릿 보호(NFR-2)
//! 훅 프로세스는 부모 환경을 상속하되(PATH 등 필요), **알려진 시크릿 env**(source `uri_env`,
//! S3 `credentials_env`, AES 키)는 제거하고 넘긴다. 컨텍스트로 주입하는 `XB_*`에는 시크릿
//! 값을 담지 않으며, `XB_ERROR`는 자격증명 URI를 마스킹한다([`mask_secrets`]).

use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

use crate::config::file::HooksConfig;
use crate::error::{Result, XBackupError};

/// 훅 타임아웃 기본값(초) — `hook_timeout_secs` 미지정 시.
const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// 셸 실행기(플랫폼별). 릴리스 대상은 darwin/linux이므로 unix는 `/bin/sh -c`.
#[cfg(unix)]
const SHELL: (&str, &str) = ("/bin/sh", "-c");
#[cfg(not(unix))]
const SHELL: (&str, &str) = ("cmd", "/C");

/// 훅 실행 이벤트(생명주기 지점).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    PreBackup,
    PostBackup,
    PreRestore,
    PostRestore,
    PrePrune,
    PostPrune,
    /// 어느 단계든 실패 시 마지막에 1회.
    OnError,
}

impl HookEvent {
    /// `XB_EVENT`로 노출되는 안정 식별자.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::PreBackup => "pre_backup",
            HookEvent::PostBackup => "post_backup",
            HookEvent::PreRestore => "pre_restore",
            HookEvent::PostRestore => "post_restore",
            HookEvent::PrePrune => "pre_prune",
            HookEvent::PostPrune => "post_prune",
            HookEvent::OnError => "on_error",
        }
    }
}

/// 훅에 주입할 컨텍스트 — 환경변수(`XB_*`)로 전달된다. 시크릿은 담지 않는다.
#[derive(Debug, Default, Clone)]
pub struct HookContext {
    /// 프로파일 이름(`XB_PROFILE`).
    pub profile: String,
    /// DB 종류 — mongodb/postgresql/mysql(`XB_ENGINE`).
    pub engine: Option<String>,
    /// 백업 유형 — full/incremental(`XB_BACKUP_TYPE`).
    pub backup_type: Option<String>,
    /// 생성/대상 백업 ID(`XB_BACKUP_ID`).
    pub backup_id: Option<String>,
    /// 결과 상태 — ok/failed(`XB_STATUS`).
    pub status: Option<String>,
    /// 종료 코드(`XB_EXIT_CODE`).
    pub exit_code: Option<i32>,
    /// 시작 시각 RFC3339(`XB_STARTED_AT`).
    pub started_at: Option<String>,
    /// 종료 시각 RFC3339(`XB_ENDED_AT`).
    pub ended_at: Option<String>,
    /// 에러 요약(`XB_ERROR`) — 주입 시 [`mask_secrets`]로 마스킹된다.
    pub error: Option<String>,
}

impl HookContext {
    /// 컨텍스트를 `XB_*` 환경변수 쌍으로 만든다(값이 있는 필드만).
    fn to_env(&self, event: HookEvent) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = vec![
            ("XB_EVENT".into(), event.as_str().into()),
            ("XB_PROFILE".into(), self.profile.clone()),
        ];
        fn push(v: &mut Vec<(String, String)>, k: &str, val: &Option<String>) {
            if let Some(s) = val.as_deref().filter(|s| !s.is_empty()) {
                v.push((k.into(), s.to_string()));
            }
        }
        push(&mut v, "XB_ENGINE", &self.engine);
        push(&mut v, "XB_BACKUP_TYPE", &self.backup_type);
        push(&mut v, "XB_BACKUP_ID", &self.backup_id);
        push(&mut v, "XB_STATUS", &self.status);
        if let Some(code) = self.exit_code {
            v.push(("XB_EXIT_CODE".into(), code.to_string()));
        }
        push(&mut v, "XB_STARTED_AT", &self.started_at);
        push(&mut v, "XB_ENDED_AT", &self.ended_at);
        if let Some(e) = self.error.as_deref().filter(|s| !s.is_empty()) {
            v.push(("XB_ERROR".into(), mask_secrets(e)));
        }
        v
    }
}

/// 해석된 훅 집합 — config `[profiles.<name>.hooks]` + 실행 정책.
pub struct HookSet {
    cfg: HooksConfig,
    /// `--no-hooks`면 false(모든 훅 비활성).
    enabled: bool,
    /// 훅 env에서 제거할 시크릿 env 변수 이름(source uri_env, s3 credentials_env 등).
    secret_env_names: Vec<String>,
}

impl HookSet {
    /// 훅 집합을 만든다. `enabled=false`면 어떤 훅도 실행하지 않는다(`--no-hooks`).
    pub fn new(cfg: HooksConfig, enabled: bool, secret_env_names: Vec<String>) -> Self {
        Self {
            cfg,
            enabled,
            secret_env_names,
        }
    }

    /// 이 이벤트에 설정된 명령(비어있지 않은 것)을 돌려준다.
    fn command_for(&self, event: HookEvent) -> Option<&str> {
        let field = match event {
            HookEvent::PreBackup => &self.cfg.pre_backup,
            HookEvent::PostBackup => &self.cfg.post_backup,
            HookEvent::PreRestore => &self.cfg.pre_restore,
            HookEvent::PostRestore => &self.cfg.post_restore,
            HookEvent::PrePrune => &self.cfg.pre_prune,
            HookEvent::PostPrune => &self.cfg.post_prune,
            HookEvent::OnError => &self.cfg.on_error,
        };
        field.as_deref().filter(|s| !s.trim().is_empty())
    }

    /// 훅 타임아웃(0/미지정은 기본값으로 폴백).
    fn timeout(&self) -> Duration {
        let secs = self
            .cfg
            .hook_timeout_secs
            .filter(|&s| s > 0)
            .unwrap_or(DEFAULT_TIMEOUT_SECS);
        Duration::from_secs(secs)
    }

    /// 게이팅 훅(`pre_*`) 실행 — 비-0 종료·실행 오류면 작업을 중단시키는 에러를 반환한다.
    /// 훅이 없거나 `--no-hooks`면 통과(Ok).
    pub async fn run_gate(&self, event: HookEvent, ctx: &HookContext) -> Result<()> {
        let Some(cmd) = self.command_for(event) else {
            return Ok(());
        };
        if !self.enabled {
            tracing::info!(hook = event.as_str(), "--no-hooks — 게이트 훅을 건너뜁니다");
            return Ok(());
        }
        let envs = ctx.to_env(event);
        match self.spawn(cmd, &envs).await {
            Ok(status) if status.success() => {
                tracing::info!(hook = event.as_str(), "게이트 훅 통과");
                Ok(())
            }
            Ok(status) => Err(XBackupError::Failure(format!(
                "{} 훅이 비-0 종료(code {})로 작업을 차단했습니다",
                event.as_str(),
                status.code().unwrap_or(-1)
            ))),
            Err(e) => Err(e),
        }
    }

    /// 관측 훅(`post_*`/`on_error`) 실행 — 실패해도 경고 로그만 남기고 반환한다(작업 판정 불변).
    pub async fn run_observe(&self, event: HookEvent, ctx: &HookContext) {
        let Some(cmd) = self.command_for(event) else {
            return;
        };
        if !self.enabled {
            return;
        }
        let envs = ctx.to_env(event);
        match self.spawn(cmd, &envs).await {
            Ok(status) if status.success() => {
                tracing::info!(hook = event.as_str(), "관측 훅 완료")
            }
            Ok(status) => tracing::warn!(
                hook = event.as_str(),
                "관측 훅이 비-0 종료(code {}) — 무시(작업은 이미 판정됨)",
                status.code().unwrap_or(-1)
            ),
            Err(e) => tracing::warn!(
                hook = event.as_str(),
                "관측 훅 실행 오류: {e} — 무시(작업은 이미 판정됨)"
            ),
        }
    }

    /// 셸 훅을 스폰해 타임아웃 안에서 완료를 기다린다. 시크릿 env를 제거하고 `XB_*`를 주입하며,
    /// stdin은 연결하지 않고(NFR-3) stdout/stderr는 캡처해 로그로 남긴다.
    async fn spawn(
        &self,
        cmd: &str,
        envs: &[(String, String)],
    ) -> Result<std::process::ExitStatus> {
        let (prog, flag) = SHELL;
        let mut command = Command::new(prog);
        command.arg(flag).arg(cmd);
        // 시크릿 env 제거(NFR-2) — 상속은 하되 알려진 비밀만 지운다.
        for name in &self.secret_env_names {
            command.env_remove(name);
        }
        command.env_remove(crate::pipeline::stage::ENV_AES_KEY_HEX);
        for (k, v) in envs {
            command.env(k, v);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true); // 타임아웃으로 future가 drop되면 직계 자식(sh)을 종료.
                                 // 자식을 **새 프로세스 그룹**의 리더로 만든다(pgid = 자식 pid). 타임아웃 시 그룹 전체에
                                 // SIGKILL을 보내 `sh -c "a && b"`가 fork한 손자까지 정리한다(kill_on_drop은 직계만 잡음).
        #[cfg(unix)]
        command.process_group(0);

        let child = command
            .spawn()
            .map_err(|e| XBackupError::Failure(format!("훅 프로세스 시작 실패: {e}")))?;
        // wait_with_output이 child를 소비하므로 pid(=pgid)를 미리 확보한다.
        #[cfg(unix)]
        let pgid = child.id();

        match timeout(self.timeout(), child.wait_with_output()).await {
            Ok(Ok(output)) => {
                log_hook_output(&output);
                Ok(output.status)
            }
            Ok(Err(e)) => Err(XBackupError::Failure(format!("훅 실행 오류: {e}"))),
            Err(_elapsed) => {
                // future drop → kill_on_drop이 직계 sh를 죽인다. 손자까지 확실히 정리하려고
                // 프로세스 그룹 전체(-pgid)에 SIGKILL을 보낸다.
                #[cfg(unix)]
                if let Some(pid) = pgid {
                    // SAFETY: 음수 인자 = 프로세스 그룹 전체. 위 process_group(0)로 우리가 만든
                    // 그룹(pgid = 이 자식 pid)만 대상이며, 반환값(이미 종료 등)은 무시해도 안전하다.
                    unsafe {
                        libc::kill(-(pid as libc::pid_t), libc::SIGKILL);
                    }
                }
                Err(XBackupError::Failure(format!(
                    "훅 타임아웃({}초 초과) — 프로세스를 종료했습니다",
                    self.timeout().as_secs()
                )))
            }
        }
    }
}

/// 훅의 stdout/stderr를 로그로 남긴다(마스킹). 결과 stdout(--json) 오염을 막기 위해 tracing만 쓴다.
fn log_hook_output(output: &std::process::Output) {
    let out = String::from_utf8_lossy(&output.stdout);
    let err = String::from_utf8_lossy(&output.stderr);
    let out = out.trim();
    let err = err.trim();
    if !out.is_empty() {
        let masked = mask_secrets(out);
        tracing::info!(
            "{}",
            crate::tr!("hook stdout: {masked}", "훅 stdout: {masked}")
        );
    }
    if !err.is_empty() {
        let masked = mask_secrets(err);
        tracing::info!(
            "{}",
            crate::tr!("hook stderr: {masked}", "훅 stderr: {masked}")
        );
    }
}

/// 문자열에서 자격증명이 담긴 URI(`scheme://user:pass@host`)의 userinfo를 `***`로 마스킹한다.
///
/// `postgresql://u:p@h/db` → `postgresql://***@h/db`. 정규식 의존 없이 스캔한다 — `"://"`를
/// 찾고, 그 뒤 authority 구간(공백·`?`·`#`·끝 이전)에서 **마지막 `@`**를 userinfo/host 경계로
/// 본다. 경계에 `/`를 넣지 않으므로, percent-encoding 안 된 password 내 `/`(예: `u:pa/ss@h`)도
/// 마스킹된다(F5). host에는 `@`가 올 수 없으므로 `@`가 있으면 곧 자격증명이다.
pub fn mask_secrets(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i..].starts_with(b"://") {
            out.push_str("://");
            i += 3;
            // authority 구간 끝 — 공백/쿼리(`?`)/프래그먼트(`#`)/끝. `/`는 경계가 아니다
            // (password에 인코딩 안 된 `/`가 올 수 있으므로). 구간 내 마지막 `@`가 userinfo 끝.
            let start = i;
            let mut end = bytes.len();
            let mut at: Option<usize> = None;
            let mut j = start;
            while j < bytes.len() {
                match bytes[j] {
                    b' ' | b'\t' | b'\n' | b'\r' | b'?' | b'#' => {
                        end = j;
                        break;
                    }
                    b'@' => at = Some(j),
                    _ => {}
                }
                j += 1;
            }
            match at.filter(|&a| a < end) {
                Some(a) => {
                    // userinfo(start..a) 마스킹, host+경로(a..end)는 보존.
                    out.push_str("***");
                    out.push_str(&s[a..end]);
                }
                None => out.push_str(&s[start..end]),
            }
            i = end;
        } else {
            // UTF-8 안전: 문자 경계 단위로 밀어 넣는다.
            let ch_len = utf8_len(bytes[i]);
            out.push_str(&s[i..(i + ch_len).min(s.len())]);
            i += ch_len;
        }
    }
    out
}

/// UTF-8 선두 바이트로 문자 길이를 판정한다(1~4).
fn utf8_len(b: u8) -> usize {
    if b < 0x80 {
        1
    } else if b >> 5 == 0b110 {
        2
    } else if b >> 4 == 0b1110 {
        3
    } else if b >> 3 == 0b11110 {
        4
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(cfg: HooksConfig) -> HookSet {
        HookSet::new(cfg, true, vec![])
    }

    fn ctx() -> HookContext {
        HookContext {
            profile: "prod".into(),
            engine: Some("postgresql".into()),
            backup_type: Some("full".into()),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn gate_passes_on_zero_exit() {
        let cfg = HooksConfig {
            pre_backup: Some("true".into()),
            ..Default::default()
        };
        set(cfg)
            .run_gate(HookEvent::PreBackup, &ctx())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn gate_blocks_on_nonzero_exit() {
        let cfg = HooksConfig {
            pre_backup: Some("exit 7".into()),
            ..Default::default()
        };
        let err = set(cfg)
            .run_gate(HookEvent::PreBackup, &ctx())
            .await
            .unwrap_err();
        // 게이트 차단은 작업 실패(exit 1)로 매핑된다.
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("차단"));
    }

    #[tokio::test]
    async fn missing_hook_is_noop_ok() {
        // 설정 없는 이벤트는 통과.
        set(HooksConfig::default())
            .run_gate(HookEvent::PreBackup, &ctx())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn disabled_gate_skips_and_passes() {
        let cfg = HooksConfig {
            pre_backup: Some("exit 1".into()), // 실행되면 실패할 명령
            ..Default::default()
        };
        // --no-hooks(enabled=false)면 실행하지 않고 통과.
        let hs = HookSet::new(cfg, false, vec![]);
        hs.run_gate(HookEvent::PreBackup, &ctx()).await.unwrap();
    }

    #[tokio::test]
    async fn observe_never_errors_on_failure() {
        let cfg = HooksConfig {
            post_backup: Some("exit 3".into()),
            ..Default::default()
        };
        // 반환 타입이 () — 실패해도 패닉/에러 없이 지나가야 한다.
        set(cfg).run_observe(HookEvent::PostBackup, &ctx()).await;
    }

    /// --no-hooks(enabled=false)면 관측 훅도 실행하지 않는다(부수효과 없음).
    #[tokio::test]
    async fn disabled_observe_does_not_run() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("ran");
        let cfg = HooksConfig {
            post_backup: Some(format!("touch '{}'", marker.display())),
            ..Default::default()
        };
        let hs = HookSet::new(cfg, false, vec![]);
        hs.run_observe(HookEvent::PostBackup, &ctx()).await;
        assert!(
            !marker.exists(),
            "--no-hooks면 관측 훅이 실행되지 않아야 함"
        );
    }

    #[tokio::test]
    async fn gate_times_out() {
        let cfg = HooksConfig {
            pre_backup: Some("sleep 5".into()),
            hook_timeout_secs: Some(1),
            ..Default::default()
        };
        let err = set(cfg)
            .run_gate(HookEvent::PreBackup, &ctx())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("타임아웃"));
    }

    #[tokio::test]
    async fn env_context_reaches_hook() {
        // XB_EVENT/XB_PROFILE이 훅 환경에 주입되는지 — 값이 다르면 exit 1로 실패시킨다.
        let cfg = HooksConfig {
            pre_backup: Some(
                "[ \"$XB_EVENT\" = pre_backup ] && [ \"$XB_PROFILE\" = prod ] && \
                 [ \"$XB_ENGINE\" = postgresql ]"
                    .into(),
            ),
            ..Default::default()
        };
        set(cfg)
            .run_gate(HookEvent::PreBackup, &ctx())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn secret_env_is_scrubbed() {
        std::env::set_var("XB_TEST_SECRET", "supersecret");
        let cfg = HooksConfig {
            // 시크릿 env가 비어 있어야(제거돼야) exit 0.
            pre_backup: Some("[ -z \"$XB_TEST_SECRET\" ]".into()),
            ..Default::default()
        };
        let hs = HookSet::new(cfg, true, vec!["XB_TEST_SECRET".into()]);
        hs.run_gate(HookEvent::PreBackup, &ctx()).await.unwrap();
        std::env::remove_var("XB_TEST_SECRET");
    }

    #[test]
    fn masks_credential_uri() {
        assert_eq!(
            mask_secrets("postgresql://user:pass@host:5432/db"),
            "postgresql://***@host:5432/db"
        );
        assert_eq!(
            mask_secrets("mongodb://a:b@h/?replicaSet=rs0"),
            "mongodb://***@h/?replicaSet=rs0"
        );
    }

    #[test]
    fn mask_leaves_non_credential_uri() {
        assert_eq!(mask_secrets("https://host/path"), "https://host/path");
        assert_eq!(mask_secrets("no uri here"), "no uri here");
    }

    /// F5: password에 인코딩 안 된 `/`가 있어도 userinfo 전체가 마스킹된다(경계가 `/`가 아님).
    #[test]
    fn mask_handles_slash_in_password() {
        assert_eq!(
            mask_secrets("postgresql://user:pa/ss@host/db"),
            "postgresql://***@host/db"
        );
        // 쿼리 스트링 경계도 유지(? 이전까지가 authority).
        assert_eq!(
            mask_secrets("mongodb://a:b@h/?replicaSet=rs0"),
            "mongodb://***@h/?replicaSet=rs0"
        );
    }

    #[test]
    fn mask_preserves_multibyte() {
        // 한글이 섞여도 깨지지 않는다.
        assert_eq!(
            mask_secrets("백업 실패 postgresql://u:p@h/db 에서"),
            "백업 실패 postgresql://***@h/db 에서"
        );
    }

    #[test]
    fn to_env_includes_masked_error() {
        let c = HookContext {
            profile: "p".into(),
            error: Some("fail postgresql://u:p@h/db".into()),
            ..Default::default()
        };
        let env = c.to_env(HookEvent::OnError);
        let err = env.iter().find(|(k, _)| k == "XB_ERROR").unwrap();
        assert_eq!(err.1, "fail postgresql://***@h/db");
    }
}
