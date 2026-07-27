//! `GET /doctor` — config 정적 점검 결과 화면.
//!
//! ## 왜 이 화면이 첫 화면인가
//! `doctor`는 **DB에 연결하지 않고 config만 읽는 오프라인 점검**이다(`src/cli/handlers/doctor.rs`
//! 헤더). 그래서 파괴적이지 않고, 락을 잡지 않고, 실패해도 아무것도 망가지지 않는다 —
//! 자식 프로세스 중계라는 이 콘솔의 기본 구조를 가장 위험이 낮은 대상으로 먼저 세울 수
//! 있는 화면이다.
//!
//! ## 점검 자식은 **잡 러너로** 띄운다 — 자체 `Command`를 만들지 않는다
//! 이전 판본은 이 파일에서 [`tokio::process::Command`]를 직접 조립했고, 서버 환경을 그대로
//! 물려주면서 세션 토큰 두 개만 `env_remove`로 뺐다(블랙리스트). 그 결정이 **웹과 CLI가
//! 다르게 동작할 수 없다**([`crate::web`] 모듈 헤더의 최상위 불변식)를 양방향으로 깼고,
//! 리뷰가 둘 다 실측했다:
//!
//! 1. **상시 오진(거짓 경고).** [`crate::web::job::JobSecrets::load`]가 기동 시점에
//!    `uri_env`/`read_uri_env`/`credentials_env`가 가리키는 env와 `XB_AES_KEY_HEX`를 서버
//!    환경에서 **제거**한다(시크릿을 상주시키지 않기 위해 — 그쪽 모듈 헤더). 상속만 받는
//!    자식은 그 제거된 env를 보므로, 정상 설정에서도 "시크릿 환경변수가 설정되지 않았습니다"
//!    (WARN)를 냈다. 같은 셸의 CLI는 `resolved → mongodb`(ok)였다. `db`가 `null`로 접히면서
//!    엔진별 점검(pg_logical 등)까지 화면에서 사라졌다.
//! 2. **반대 방향의 오진(거짓 초록).** [`crate::web::job::runner::JobRunner`]는 화이트리스트로
//!    `XB_*__*` config 오버라이드를 **의도적으로 뺀다**(그쪽 헤더 "자식 env는 화이트리스트로
//!    새로 짓는다"). 블랙리스트인 이 파일은 그것을 상속했다. 그래서 셸에 `XB_SOURCE__URI`가
//!    export되어 있으면 이 화면은 초록인데 같은 프로파일의 백업 잡은 exit 2로 끊겼다 —
//!    운영자가 초록 화면을 보고 백업을 눌러 실패를 받는다. **이쪽이 더 위험하다.**
//!
//! 두 오진의 원인이 하나(자식 env가 잡과 다르다)이므로 해법도 하나다: 점검도 잡과 **같은
//! 경로로** 띄운다. [`crate::web::ServeConfig::jobs`]의 러너에 `JobSpec::new(JobCommand::Doctor, lang)`
//! 를 넘기면 `env_clear` + 화이트리스트 + 시크릿 주입 + `--config`/`--lang`이 전부 잡과
//! 동일하게 적용된다. 이 파일은 이제 "무엇을 실행할지"를 정하지 않는다 — 명세 하나만 만든다.
//!
//! `doctor`는 파괴적이지 않으므로 [`JobRunner::spawn`]으로 충분하다(감사 게이트를 지나는
//! [`spawn_destructive`](crate::web::job::JobRunner::spawn_destructive)가 아니다) — 읽기 전용
//! 점검을 감사 로그에 남기면 그 로그가 점검 기록으로 오염된다.
//!
//! ### 그래도 이 파일에 남는 것: 요청 수명에 묶인 유한 실행
//! 잡 러너는 `kill_on_drop`을 **의도적으로 켜지 않는다**(2시간짜리 백업이 배포나 핸들러의
//! 조기 반환으로 죽으면 안 된다 — 그쪽 헤더). 하지만 `doctor`는 요청 하나 안에서 끝나야
//! 하는 짧은 잡이므로 상한이 필요하다([`DOCTOR_TIMEOUT`]). 그래서 상한 초과 시
//! [`await_with_limit`]이 pid로 직접 `SIGKILL`을 보낸다 — 러너의 정책은 건드리지 않고,
//! 이 화면에만 필요한 "유한 실행"을 호출부에서 얹는다.
//!
//! ## 왜 PATH를 타지 않는가
//! `Command::new("x-backup")`은 **PATH를 탐색한다.** 그러면 이 서버를
//! `/opt/x-backup/bin/x-backup serve`로 띄웠는데 점검은 PATH 앞쪽의 낡은
//! `/usr/local/bin/x-backup`이 수행하는 상황이 조용히 벌어진다 — 버전이 다르면 JSON 스키마도
//! 다르고, 최악의 경우 공격자가 심어 둔 동명 바이너리가 실행된다. 잡 러너가 이미
//! [`std::env::current_exe`]를 들고 있으므로(그쪽 헤더의 같은 근거) 그 성질을 그대로
//! 물려받는다. [`doctor_exe`]는 남아 있다 — 아직 디스크에 없는 **후보** config를 검증하는
//! [`crate::web::config_write`]가 러너를 쓸 수 없어(그 모듈 헤더 참조) 같은 근거를 자기
//! 손으로 반복해야 하기 때문이다.
//!
//! ## 시크릿
//! `doctor --json`의 출력에는 설계상 자격증명 *값*이 없다 — config에는 env 변수 *이름*만
//! 들어가고(PRD §11) 값은 런타임 env에 있다. 그래도 자식 stdout/stderr는 우리가 형태를
//! 통제할 수 없는 텍스트이므로 [`crate::web::mask::SecretRegistry`]를 한 번 통과시킨다
//! (2차 방어 — `src/web/mask.rs` 헤더 참고).
//!
//! 그 레지스트리는 **잡 러너가 실제로 자식에게 주입하는 값들**이어야 한다
//! ([`request_registry`]). 이전 판본은 `register_from_env_names([XB_AES_KEY_HEX, XB_WEB_TOKEN])`
//! 로 env에서 값을 다시 읽었는데, `XB_AES_KEY_HEX`는 `JobSecrets::load`가 기동 시점에 이미
//! 제거했으므로 **그 등록은 항상 0건이었다** — 형제 라우트(`routes::jobs`·`routes::backup`)는
//! `ctx.jobs.secret_registry()`를 쓰는데 이 화면만 빠져 있었고, 리뷰가 destination 경로에 AES
//! 키 hex를 심어 원문 노출을 실측했다.
//!
//! 반대로 **서버 자신의 내부 경로**(state 디렉터리, config 파일 위치)는 애초에 마크업에
//! 싣지 않는다 — 마스킹의 대상이 아니라 부재의 대상이다.
//!
//! ## 함정
//! - `doctor`의 exit 4는 **경고 동반 성공**이다(`src/error.rs` `exit_codes::WARNING`). 실패로
//!   칠하면 운영자가 멀쩡한 설정을 고치려 든다. 0/4/3은 서로 다른 [`Level`]로 그린다.
//! - `doctor`는 config 없이는 exit 2(Usage)로 끝난다. 그건 "설정이 나쁘다"가 아니라 "서버에
//!   config가 연결되지 않았다"이므로 별도 판정([`Verdict::Misconfigured`])으로 가른다.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, State};
use maud::{html, Markup};
use serde::Deserialize;

use crate::cli::handlers::doctor::DOCTOR_JSON_SCHEMA;
use crate::error::exit_codes;
use crate::i18n::Lang;
use crate::web::auth::AuthState;
use crate::web::job::{JobCommand, JobRunner, JobSpec, RunningJob};
use crate::web::jsonguard;
use crate::web::mask::SecretRegistry;
use crate::web::view::components::{self, Level};
use crate::web::view::layout;
use crate::web::ServeConfig;

/// 화면 경로. 라우터·마크업·테스트가 이 상수를 공유한다.
pub const DOCTOR_PATH: &str = "/doctor";

/// `<title>`·화면 제목·내비게이션 라벨이 공유하는 이름. 기술용어이므로 영문 고정
/// ([`crate::i18n`] 규약). [`crate::web::view::layout`]이 현재 화면 판정에 이 값을 쓴다.
pub const DOCTOR_TITLE: &str = "Doctor";

/// 자식 프로세스 실행 상한.
///
/// ## 20초의 근거
/// `doctor`가 하는 일은 **config 파싱 + 몇 번의 `stat()`**뿐이다 — DB 연결도, 네트워크
/// 호출도 설계상 없다(연결 점검은 `status`의 몫). 로컬에서 프로파일 5개짜리 config가
/// 수십 밀리초에 끝나므로 20초는 관측값의 수백 배다. 그럼에도 0이 아닌 상한이 필요한
/// 이유는 `recipient_file`이 NFS·SMB처럼 멈출 수 있는 마운트에 있을 때
/// `Path::exists()`가 실제로 오래 매달릴 수 있다는 것이다.
///
/// 위쪽 경계는 앞단 리버스 프록시가 정한다 — nginx `proxy_read_timeout` 기본값이 60초이므로,
/// 그보다 넉넉히 짧게 잡아야 **우리가 만든 읽을 수 있는 오류 화면**이 나가고 프록시의 504
/// 기본 페이지가 나가지 않는다. 20초는 그 두 경계(관측 수십 ms ↔ 프록시 60s) 사이에서
/// 사람이 브라우저 앞에서 기다릴 수 있는 상한이기도 하다.
///
/// 상한에 걸리면 [`await_with_limit`]이 자식에게 `SIGKILL`을 보낸다 — 무한 대기를 남기지
/// 않는다.
const DOCTOR_TIMEOUT: Duration = Duration::from_secs(20);

/// 오류 화면에 붙이는 자식 stderr·파서 메시지 발췌 상한(문자 수).
///
/// 자식 stderr는 길이 제한이 없다. 통째로 그리면 오류 화면이 스크롤 지옥이 되고, 정작
/// 필요한 첫 줄(무엇이 실패했는가)이 묻힌다. 400자는 `tracing` 한 줄 + 스택 힌트 정도가
/// 들어가는 폭이다.
const EXCERPT_CHARS: usize = 400;

// ---------------------------------------------------------------------------
// 판정 — 자식 종료 코드를 화면 상태로 접는다 (순수)
// ---------------------------------------------------------------------------

/// 자식 종료 코드의 화면 판정.
///
/// exit code를 그대로 화면에 흘리지 않고 이 열거형을 거치는 이유: `doctor`의 0/4/3은 각각
/// **의미가 다른 성공/성공/실패**이고, 그 의미를 아는 곳이 한 군데(여기)여야 화면마다 다르게
/// 해석되는 사고가 없다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// exit 0 — 점검 통과.
    Clean,
    /// exit 4 — **경고 동반 성공.** 백업은 계속 돌아간다.
    Warned,
    /// exit 3 — 차단성 설정 문제. 백업 전에 고쳐야 한다.
    Blocked,
    /// exit 2 — `doctor`가 config를 받지 못했다(서버 기동에 `--config`가 없었다).
    Misconfigured,
    /// 그 밖의 종료. `None`은 시그널로 죽은 경우(코드 없음).
    Unexpected(Option<i32>),
}

impl Verdict {
    /// 종료 코드를 판정으로 접는다. `None`은 시그널 종료(유닉스에서 코드가 없는 경우).
    pub fn from_exit(code: Option<i32>) -> Self {
        match code {
            Some(c) if c == i32::from(exit_codes::SUCCESS) => Verdict::Clean,
            Some(c) if c == i32::from(exit_codes::WARNING) => Verdict::Warned,
            Some(c) if c == i32::from(exit_codes::PRECHECK) => Verdict::Blocked,
            Some(c) if c == i32::from(exit_codes::USAGE) => Verdict::Misconfigured,
            other => Verdict::Unexpected(other),
        }
    }

    /// 화면 색·배지를 결정하는 의미 레벨.
    ///
    /// `Warned`가 [`Level::Warn`]이고 [`Level::Fail`]이 **아닌** 것이 이 함수의 요점이다 —
    /// exit 4는 성공이다.
    pub fn level(self) -> Level {
        match self {
            Verdict::Clean => Level::Ok,
            Verdict::Warned => Level::Warn,
            Verdict::Blocked => Level::Fail,
            // 둘 다 "점검 결과를 얻지 못했다" — config가 나쁜 게 아니라 점검이 성립하지 않았다.
            Verdict::Misconfigured | Verdict::Unexpected(_) => Level::Error,
        }
    }

    /// 배너 헤드라인 — 무슨 일이 일어났는지 한 문장(설명 텍스트이므로 ko/en 토글).
    pub fn headline(self, lang: Lang) -> String {
        match self {
            Verdict::Clean => lang
                .sel("All checks passed.", "모든 점검을 통과했습니다.")
                .to_string(),
            Verdict::Warned => lang
                .sel(
                    "Passed with warnings.",
                    "경고와 함께 통과했습니다(성공입니다).",
                )
                .to_string(),
            Verdict::Blocked => lang
                .sel(
                    "Blocking configuration problem found.",
                    "차단성 설정 문제가 있습니다.",
                )
                .to_string(),
            Verdict::Misconfigured => lang
                .sel(
                    "doctor had no config to check.",
                    "doctor가 점검할 config를 받지 못했습니다.",
                )
                .to_string(),
            Verdict::Unexpected(Some(code)) => format!(
                "{} (exit {code})",
                lang.sel(
                    "doctor exited unexpectedly",
                    "doctor가 예상치 못한 코드로 끝났습니다"
                )
            ),
            Verdict::Unexpected(None) => lang
                .sel(
                    "doctor was killed by a signal.",
                    "doctor가 시그널로 종료됐습니다.",
                )
                .to_string(),
        }
    }

    /// 배너 두 번째 줄 — 근거(종료 코드)와 다음 행동. 없으면 `None`.
    pub fn detail(self, lang: Lang) -> Option<String> {
        let text = match self {
            Verdict::Clean => lang.sel(
                "exit 0 — nothing to do.",
                "exit 0 — 조치할 것이 없습니다.",
            ),
            Verdict::Warned => lang.sel(
                "exit 4 — warnings are success with caveats: backups still run. Review the WARN rows.",
                "exit 4 — 경고는 '단서가 붙은 성공'입니다. 백업은 계속 동작합니다. 아래 WARN 행을 확인하세요.",
            ),
            Verdict::Blocked => lang.sel(
                "exit 3 — fix the FAIL rows before the next backup; the run will refuse to start.",
                "exit 3 — 아래 FAIL 항목을 고치기 전에는 백업이 시작을 거부합니다.",
            ),
            Verdict::Misconfigured => lang.sel(
                "exit 2 — restart the console with --config <PATH> (or XB_CONFIG) so doctor has something to read.",
                "exit 2 — 콘솔을 --config <PATH>(또는 XB_CONFIG)와 함께 다시 띄우면 doctor가 읽을 대상이 생깁니다.",
            ),
            Verdict::Unexpected(_) => return None,
        };
        Some(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// 파싱 — 자식 stdout → 보고서 (순수)
// ---------------------------------------------------------------------------

/// `doctor --json`의 최상위 문서. 필드 이름은 `src/cli/handlers/doctor.rs::render_json`과
/// 1:1로 맞춘다 — 한쪽만 바꾸면 [`ReportError::Malformed`]로 즉시 드러난다.
#[derive(Debug, Deserialize)]
pub struct Report {
    /// 스키마 버전. [`parse_report`]가 [`DOCTOR_JSON_SCHEMA`]와 일치하는지 먼저 본다.
    pub schema: u32,
    /// 전체 판정 문자열(`ok`/`warn`/`fail`). 화면 판정은 종료 코드가 권위이고, 이 값은
    /// 교차 확인용으로 메타에 그대로 표시한다.
    pub overall: String,
    /// 프로파일별 결과(자식이 이름순으로 정렬해 내보낸다).
    pub profiles: Vec<ProfileReport>,
}

/// 프로파일 하나의 점검 결과.
#[derive(Debug, Deserialize)]
pub struct ProfileReport {
    /// 프로파일 이름(config의 `[profiles.<name>]`).
    pub profile: String,
    /// 엔진 라벨(`postgresql`/`mysql`/`mongodb`). URI를 해석하지 못하면 `null`.
    pub db: Option<String>,
    /// 점검 항목.
    pub items: Vec<Item>,
}

/// 점검 항목 하나.
#[derive(Debug, Deserialize)]
pub struct Item {
    /// 점검 이름(`source`/`destination`/`encryption`/…). 영문 라벨 고정.
    pub label: String,
    /// `ok`/`warn`/`fail`. 모르는 값은 [`level_from_status`]가 [`Level::Error`]로 떨어뜨린다.
    pub status: String,
    /// 사람이 읽는 설명. **자식이 만든 남의 텍스트**이므로 마스킹·이스케이프 대상이다.
    pub message: String,
}

/// stdout을 보고서로 접지 못한 이유.
///
/// 각 변종이 따로 있는 이유는 오류 화면이 "무엇을 의심해야 하는지" 다르게 안내해야 하기
/// 때문이다 — 빈 출력은 자식이 죽은 쪽을 의심하고, 스키마 불일치는 바이너리 버전 섞임을
/// 의심한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// stdout이 비었다(자식이 아무것도 못 찍고 끝났다).
    Empty,
    /// stdout이 UTF-8이 아니다.
    NotUtf8,
    /// JSON이 아니거나 기대한 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 이 빌드가 아는 값과 다르다.
    SchemaMismatch { found: u64, expected: u32 },
    /// 중첩이 너무 깊어 **파싱을 시도하지 않았다**([`crate::web::jsonguard`]).
    ///
    /// `Malformed`와 따로 두는 이유는 이 enum의 원칙 그대로다 — 의심할 곳이 다르다.
    /// `Malformed`는 자식이 깨진 JSON을 냈다는 뜻이지만, 이쪽은 JSON이 문법적으로는
    /// 멀쩡할 수 있고 **데이터가 병리적**이라는 뜻이다.
    TooDeep { found: usize, max: usize },
}

impl ReportError {
    /// 오류 화면에 그대로 들어가는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            ReportError::Empty => lang
                .sel(
                    "doctor produced no output. The child may have died before writing anything.",
                    "doctor가 아무 출력도 내지 않았습니다. 자식 프로세스가 쓰기 전에 죽었을 수 있습니다.",
                )
                .to_string(),
            ReportError::NotUtf8 => lang
                .sel(
                    "doctor output was not valid UTF-8, so it cannot be text at all.",
                    "doctor 출력이 UTF-8이 아닙니다 — 애초에 텍스트가 아닙니다.",
                )
                .to_string(),
            ReportError::Malformed(detail) => format!(
                "{} {}",
                lang.sel(
                    "doctor output could not be parsed as the expected JSON:",
                    "doctor 출력을 기대한 JSON으로 해석할 수 없습니다:",
                ),
                excerpt(detail)
            ),
            ReportError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} ≠ {expected})",
                lang.sel(
                    "doctor reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "doctor가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            ReportError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "doctor output is nested more deeply than this console parses. The console and the CLI are probably different builds.",
                    "doctor 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다. 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
        }
    }
}

/// 자식 stdout 바이트를 [`Report`]로 접는다.
///
/// **스키마 버전을 본문 역직렬화보다 먼저 본다.** 그래야 나중에 스키마 2가 필드를 개편했을
/// 때 "필드가 이상하다"(Malformed)가 아니라 "버전이 다르다"(SchemaMismatch)로 진단된다 —
/// 운영자가 봐야 할 곳이 config가 아니라 배포 버전이라는 것을 화면이 바로 말해준다.
///
/// 그보다도 먼저 보는 것이 중첩 깊이다([`crate::web::jsonguard`]) — 스키마 판정도
/// 역직렬화를 거쳐야 하는데, 깊은 입력은 그 역직렬화 도중에 프로세스를 죽인다.
pub fn parse_report(stdout: &[u8]) -> Result<Report, ReportError> {
    let text = std::str::from_utf8(stdout).map_err(|_| ReportError::NotUtf8)?;
    if text.trim().is_empty() {
        return Err(ReportError::Empty);
    }
    jsonguard::check_depth(text).map_err(|d| ReportError::TooDeep {
        found: d.found,
        max: d.max,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ReportError::Malformed(e.to_string()))?;
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(found) if found == u64::from(DOCTOR_JSON_SCHEMA) => {}
        Some(found) => {
            return Err(ReportError::SchemaMismatch {
                found,
                expected: DOCTOR_JSON_SCHEMA,
            })
        }
        None => {
            return Err(ReportError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }
    serde_json::from_value(value).map_err(|e| ReportError::Malformed(e.to_string()))
}

/// 항목 상태 문자열을 화면 레벨로 접는다.
///
/// 모르는 값을 [`Level::Ok`]로 떨어뜨리지 않는 것이 핵심이다 — 미래의 CLI가 새 상태
/// (`skipped` 등)를 추가했을 때 그것이 조용히 초록으로 칠해지면 화면이 거짓말을 한다.
/// 모르면 "모른다"([`Level::Error`])고 말한다.
pub fn level_from_status(raw: &str) -> Level {
    match raw {
        "ok" => Level::Ok,
        "warn" => Level::Warn,
        "fail" => Level::Fail,
        _ => Level::Error,
    }
}

/// 레벨의 심각도 순위 — 프로파일 카드에 붙일 "가장 나쁜 상태"를 고르는 데 쓴다.
fn severity(level: Level) -> u8 {
    match level {
        Level::Ok => 0,
        Level::Warn => 1,
        Level::Fail => 2,
        // 해석 못 한 상태가 가장 위험하다 — 무엇인지 모르는 것을 경고보다 낮게 볼 이유가 없다.
        Level::Error => 3,
    }
}

/// 항목들 중 가장 심각한 레벨. 항목이 없으면 [`Level::Error`](점검이 성립하지 않았다).
fn worst_level(items: &[Item]) -> Level {
    items
        .iter()
        .map(|i| level_from_status(&i.status))
        .max_by_key(|l| severity(*l))
        .unwrap_or(Level::Error)
}

/// 항목 개수를 레벨별로 센다. 카드 제목의 밀도 있는 요약(`4 checks · 3 ok · 1 warn`)에 쓴다.
fn count_by_level(items: &[Item]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for item in items {
        counts[usize::from(severity(level_from_status(&item.status)))] += 1;
    }
    counts
}

/// 문자열을 [`EXCERPT_CHARS`]로 자른다. 문자 경계로 자르므로 멀티바이트가 깨지지 않는다.
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(EXCERPT_CHARS).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------
// 자식 실행 — 잡 러너에 명세를 넘기고, 요청 수명에 맞는 상한만 얹는다 (모듈 헤더 참조)
// ---------------------------------------------------------------------------

/// 자식 프로세스를 돌리지 못한 이유.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunError {
    /// 자기 실행 파일 경로를 알 수 없다.
    Exe(String),
    /// spawn 실패(실행 권한·파일 없음 등).
    Spawn(String),
    /// 자식을 기다리는 중 I/O 오류.
    Wait(String),
    /// 상한 시간 초과 — 자식은 죽였다.
    Timeout(Duration),
}

impl RunError {
    /// 오류 화면에 그대로 들어가는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            RunError::Exe(detail) => format!(
                "{} {}",
                lang.sel(
                    "The console could not locate its own executable, so it cannot run doctor:",
                    "콘솔이 자기 실행 파일 경로를 알 수 없어 doctor를 띄울 수 없습니다:",
                ),
                excerpt(detail)
            ),
            RunError::Spawn(detail) => format!(
                "{} {}",
                lang.sel(
                    "Spawning the doctor child failed:",
                    "doctor 자식 프로세스를 띄우지 못했습니다:",
                ),
                excerpt(detail)
            ),
            RunError::Wait(detail) => format!(
                "{} {}",
                lang.sel(
                    "Reading the doctor child's output failed:",
                    "doctor 자식 프로세스의 출력을 읽는 중 실패했습니다:",
                ),
                excerpt(detail)
            ),
            RunError::Timeout(limit) => format!(
                "{} ({}s)",
                lang.sel(
                    "doctor did not finish within the time limit and was killed. A stalled network mount under recipient_file is the usual cause.",
                    "doctor가 상한 시간 안에 끝나지 않아 종료시켰습니다. recipient_file이 멈춘 네트워크 마운트에 있는 경우가 흔한 원인입니다.",
                ),
                limit.as_secs()
            ),
        }
    }
}

/// 자식 실행 결과(종료 코드 + 표준 스트림).
///
/// 스트림이 `Vec<u8>`이 아니라 `String`인 이유: 잡 러너가 이미 lossy 변환을 마친 값을
/// 돌려준다([`crate::web::job::JobCompletion`] — 자식이 비-UTF-8을 뱉어도 종료 코드를
/// 잃지 않기 위한 그쪽 결정이다). 그래서 [`ReportError::NotUtf8`]은 이 경로로는 나올 수
/// 없다 — 그 변종은 [`parse_report`]를 직접 부르는 다른 호출자([`crate::web::config_write`])
/// 와 그 단위 테스트를 위해 남는다.
#[derive(Debug)]
pub struct ChildRun {
    /// 종료 코드. `None`은 시그널 종료.
    pub exit: Option<i32>,
    /// 표준 출력(JSON이 여기 온다).
    pub stdout: String,
    /// 표준 오류(`tracing` 로그와 오류 메시지가 여기 온다).
    pub stderr: String,
}

/// 점검을 수행할 실행 파일 — **항상 자기 자신**(모듈 헤더 "왜 PATH를 타지 않는가" 참조).
///
/// 이 화면은 더 이상 이 함수를 쓰지 않는다(잡 러너가 자기 exe를 들고 있다). 남아 있는 이유는
/// [`crate::web::config_write`]가 아직 디스크에 없는 후보 config를 검증할 때 같은 근거를
/// 자기 손으로 반복해야 하기 때문이다.
pub fn doctor_exe() -> Result<PathBuf, RunError> {
    std::env::current_exe().map_err(|e| RunError::Exe(e.to_string()))
}

/// 이 화면이 실행할 잡 명세 — `doctor --json` 하나뿐이다.
///
/// `--config`/`--lang`은 여기 없다. 잡 러너가 기동 시점에 확정한 값으로 **모든** 잡에
/// 똑같이 붙이므로(그쪽 `build_command`), 이 화면이 따로 붙이면 "점검이 본 config"와
/// "잡이 볼 config"가 갈라질 수 있다 — 그 갈라짐이 이 파일이 방금 고친 버그의 본질이다.
fn doctor_spec(lang: Lang) -> JobSpec {
    JobSpec::new(JobCommand::Doctor, lang)
}

/// 점검 자식을 띄워 상한 시간 안에 끝까지 돌린다.
async fn run_doctor(runner: &JobRunner, limit: Duration, lang: Lang) -> Result<ChildRun, RunError> {
    let running = runner
        .spawn(&doctor_spec(lang))
        .map_err(|e| RunError::Spawn(e.to_string()))?;
    await_with_limit(running, limit).await
}

/// 돌고 있는 자식을 상한까지만 기다린다. 상한을 넘으면 `SIGKILL`을 보내고 [`RunError::Timeout`].
///
/// ## 왜 `kill_on_drop`이 아니라 명시적 kill인가
/// 잡 러너는 `kill_on_drop`을 **의도적으로 켜지 않는다** — 2시간짜리 백업이 배포(SIGTERM)나
/// 핸들러의 조기 반환 때마다 죽으면 안 되기 때문이다([`crate::web::job::runner`] 헤더). 그
/// 정책을 이 화면 때문에 뒤집을 수는 없으므로, "요청 하나 안에서 끝나야 하는 짧은 잡"에
/// 필요한 상한만 호출부에서 얹는다.
///
/// 프로세스 **그룹**이 아니라 pid 하나에 보낸다. `doctor`는 DB에 연결하지 않고 외부 도구를
/// 띄우지 않으므로(오프라인 점검) 정리할 손자가 없고, 그룹 시그널(`kill(-pid, …)`)은 환경에
/// 따라 `EPERM`으로 거부되는 사례가 이 프로젝트에서 이미 관측됐다. 정리할 대상이 없는데
/// 실패할 수 있는 방법을 고를 이유가 없다([`crate::web::config_write::run_with_timeout`]과
/// 같은 판단).
///
/// kill 이후 이 자식을 `wait`하는 코드는 없다 — 퓨처가 드롭되면서 tokio의 orphan 큐가
/// `SIGCHLD`에 맞춰 수거한다. 반대로 **클라이언트가 연결을 끊어 이 퓨처 자체가 드롭되는**
/// 경로에서는 자식이 남아 스스로 끝날 때까지 돈다(kill할 기회가 없다). `doctor`는 락도
/// 잡지 않고 아무것도 바꾸지 않는 읽기 전용 점검이라 그 잔여를 감수한다.
async fn await_with_limit(running: RunningJob, limit: Duration) -> Result<ChildRun, RunError> {
    let pid = running.pid();
    match tokio::time::timeout(limit, running.wait_with_output()).await {
        Ok(Ok(done)) => Ok(ChildRun {
            exit: done.outcome.exit_code(),
            stdout: done.stdout,
            stderr: done.stderr,
        }),
        Ok(Err(e)) => Err(RunError::Wait(e.to_string())),
        Err(_) => {
            kill_child(pid);
            Err(RunError::Timeout(limit))
        }
    }
}

/// 상한을 넘긴 자식에게 `SIGKILL`을 보낸다. pid를 모르면(이미 수거됨) 할 일이 없다.
#[cfg(unix)]
fn kill_child(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    // pid를 `as`로 캐스팅하지 않는다 — 변환이 실패해 -1이 되면 `kill(-1, …)`은 "이 사용자가
    // 보낼 수 있는 모든 프로세스"를 뜻한다. 실패하면 아무것도 하지 않는 편이 맞다.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: pid는 방금 우리가 spawn한, 아직 수거되지 않은 자식의 pid다(러너가 std에서
    // 받은 값). 값 자체를 신뢰할 수 없는 외부 입력이 아니므로 시그널 전송은 안전하다.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// 비-unix 플랫폼 스텁 — 이 프로젝트의 배포 대상은 darwin/linux뿐이다(release 스킬 참조).
#[cfg(not(unix))]
fn kill_child(_pid: Option<u32>) {}

// ---------------------------------------------------------------------------
// 결과 조립 + 렌더
// ---------------------------------------------------------------------------

/// 화면이 그릴 최종 상태.
#[derive(Debug)]
pub enum Outcome {
    /// 자식이 끝났고 stdout을 보고서로 읽었다.
    Reported { verdict: Verdict, report: Report },
    /// 자식은 끝났지만 stdout을 읽을 수 없었다. 종료 코드는 여전히 의미가 있으므로 함께 싣는다.
    Unreadable {
        verdict: Verdict,
        error: ReportError,
        stderr: String,
    },
    /// 자식을 돌리지 못했다(spawn 실패·타임아웃 등).
    Unavailable { error: RunError },
}

/// 자식을 띄워 결과를 모은다. **여기가 이 모듈의 유일한 부작용 지점이다.**
///
/// `--config`를 여기서 붙이지 않는다 — 러너가 기동 시점의 경로로 붙인다(모듈 헤더). 그
/// 경로가 없으면 자식이 exit 2로 끝나고 [`Verdict::Misconfigured`]가 상황을 화면에 설명한다.
/// 임의의 기본 경로를 추측해 넣지 않는다(엉뚱한 config를 점검해 보여주는 것이 최악이다).
async fn collect(cfg: &ServeConfig) -> Outcome {
    let run = match run_doctor(&cfg.jobs, DOCTOR_TIMEOUT, cfg.lang).await {
        Ok(r) => r,
        Err(e) => return Outcome::Unavailable { error: e },
    };
    let verdict = Verdict::from_exit(run.exit);
    match parse_report(run.stdout.as_bytes()) {
        Ok(report) => Outcome::Reported { verdict, report },
        Err(error) => Outcome::Unreadable {
            verdict,
            error,
            // stderr는 남의 텍스트다 — 발췌하고, 렌더에서 마스킹까지 한 번 더 통과한다.
            stderr: excerpt(&run.stderr),
        },
    }
}

/// 이 요청에서 쓸 시크릿 레지스트리.
///
/// **잡 러너의 레지스트리를 그대로 복제해 쓴다** — 자식에게 실제로 주입되는 값들이 곧
/// 자식 출력에 섞일 수 있는 값들이고, 형제 라우트(`routes::jobs`·`routes::backup`)도 같은
/// 것을 쓴다. 이전 판본이 env에서 값을 다시 읽어 등록이 항상 0건이 됐던 이유는 모듈 헤더
/// "시크릿" 참조.
///
/// 여기에 하나를 더 얹는다: **콘솔 세션 토큰.** 그 값은 잡 자식에게 주입되지 않으므로
/// (러너 화이트리스트에 없다) 러너 레지스트리에도 없지만, 우리 프로세스가 그리는 텍스트에
/// 우연히 섞이는 경로(오류 메시지 등)를 막을 값어치가 있다. [`AuthState`]에서 직접 받으므로
/// `XB_WEB_TOKEN`(env)든 `XB_WEB_TOKEN_FILE`(파일)든 출처와 무관하게 등록된다 — env만 읽던
/// 이전 판본은 파일 경로로 설정한 배포에서 이 방어가 통째로 비어 있었다.
///
/// 등록 대상을 넓히지 않는 이유는 그대로다: "XB_로 시작하는 값 전부"처럼 잡으면
/// `XB_DESTINATION__PATH=/srv/backups/...` 같은 무해한 값까지 `[REDACTED]`로 덮여 정작 점검
/// 결과를 읽을 수 없게 된다 — 마스킹이 판독성을 잡아먹는 순간 운영자는 화면을 안 보고
/// CLI로 돌아간다(그게 더 나쁘다).
fn request_registry(ctx: &ServeConfig, auth: Option<&AuthState>) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    if let Some(auth) = auth {
        auth.register_session_secret(&mut registry);
    }
    registry
}

/// `GET /doctor` 핸들러.
///
/// `Extension<Arc<AuthState>>`는 세션 토큰을 마스킹 대상으로 등록하기 위한 것이다
/// ([`request_registry`]). 라우터 전체에 걸린 Extension이므로 라우터 등록
/// (`src/web/server.rs`)은 손대지 않는다.
pub async fn page(
    State(ctx): State<Arc<ServeConfig>>,
    Extension(auth): Extension<Arc<AuthState>>,
) -> Markup {
    let outcome = collect(&ctx).await;
    let body = render(
        ctx.lang,
        ctx.config_path.is_some(),
        &outcome,
        &request_registry(&ctx, Some(&auth)),
    );
    layout::shell(ctx.lang, DOCTOR_TITLE, body)
}

/// 화면 본문을 만든다 — **순수 함수**(자식도, 파일시스템도 건드리지 않는다).
///
/// `config_present`는 config 파일의 **존재 여부만** 받는다. 경로 자체는 서버 내부 정보이므로
/// 마크업에 싣지 않는다(모듈 헤더 "시크릿" 참조).
pub fn render(
    lang: Lang,
    config_present: bool,
    outcome: &Outcome,
    registry: &SecretRegistry,
) -> Markup {
    let subtitle = lang.sel(
        "Offline configuration check — no database connection is made.",
        "오프라인 설정 점검 — DB에 연결하지 않습니다.",
    );
    html! {
        (components::page_head(DOCTOR_TITLE, Some(subtitle)))
        @match outcome {
            Outcome::Reported { verdict, report } => {
                (components::verdict_banner(verdict.level(), &verdict.headline(lang), verdict.detail(lang).as_deref()))
                (report_meta(lang, config_present, report))
                @for profile in &report.profiles {
                    (profile_panel(lang, profile, registry))
                }
            }
            Outcome::Unreadable { verdict, error, stderr } => {
                (components::verdict_banner(Level::Error, &error.explain(lang), verdict.detail(lang).as_deref()))
                (unreadable_notice(lang, *verdict, stderr, registry))
            }
            Outcome::Unavailable { error } => {
                (components::verdict_banner(Level::Error, &error.explain(lang), None))
                (components::notice(Level::Error, lang.sel("What to check", "확인할 것"), html! {
                    p { (lang.sel(
                        "The console runs checks with its own executable. Verify that the binary is still readable and executable, then retry.",
                        "콘솔은 자기 실행 파일로 점검을 수행합니다. 바이너리가 여전히 읽기·실행 가능한지 확인한 뒤 다시 시도하세요.",
                    )) }
                }))
            }
        }
    }
}

/// 보고서 상단 메타 — "몇 개를 봤고, 어떤 엔진이 섞여 있는가".
fn report_meta(lang: Lang, config_present: bool, report: &Report) -> Markup {
    let mut engines: Vec<&str> = report
        .profiles
        .iter()
        .filter_map(|p| p.db.as_deref())
        .collect();
    engines.sort_unstable();
    engines.dedup();
    let engine_text = if engines.is_empty() {
        lang.sel("(none resolved)", "(해석된 것 없음)").to_string()
    } else {
        engines.join(", ")
    };
    // config 경로가 아니라 존재 여부만 말한다 — 경로는 서버 내부 정보다.
    let config_text = if config_present {
        lang.sel("wired", "연결됨")
    } else {
        lang.sel("not wired", "연결 안 됨")
    };
    components::meta_list(&[
        ("profiles", report.profiles.len().to_string()),
        ("engines", engine_text),
        ("overall", report.overall.clone()),
        ("schema", report.schema.to_string()),
        ("config", config_text.to_string()),
    ])
}

/// 프로파일 카드 하나 — 제목줄(이름·엔진·요약·최악 상태) + 항목 표.
fn profile_panel(lang: Lang, profile: &ProfileReport, registry: &SecretRegistry) -> Markup {
    let worst = worst_level(&profile.items);
    let counts = count_by_level(&profile.items);
    // 0인 칸도 남긴다 — 열 위치가 고정되어야 카드 여러 장을 위아래로 훑을 때 눈이 안 흔들린다.
    let summary = format!(
        "{} checks · {} ok · {} warn · {} fail",
        profile.items.len(),
        counts[0],
        counts[1],
        counts[2]
    );
    let head = html! {
        h3 class="panel__title mono" { (profile.profile) }
        span class="tag" {
            @match &profile.db {
                Some(db) => (db),
                None => (lang.sel("engine unknown", "엔진 미확인")),
            }
        }
        span class="counts" { (summary) }
        (components::badge(worst))
    };
    components::panel(head, items_table(lang, &profile.items, registry))
}

/// 점검 항목 표. 행에도 `data-level`을 실어 CSS가 행 전체를 물들일 수 있게 한다 —
/// 배지 하나만으로는 행이 20개를 넘어갈 때 눈이 상태를 잃는다.
fn items_table(lang: Lang, items: &[Item], registry: &SecretRegistry) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "Check" }
                        th scope="col" { "Detail" }
                    }
                }
                tbody {
                    @if items.is_empty() {
                        tr {
                            td colspan="3" class="muted" {
                                (lang.sel("No checks were reported for this profile.", "이 프로파일에 대해 보고된 점검이 없습니다."))
                            }
                        }
                    }
                    @for item in items {
                        @let level = level_from_status(&item.status);
                        tr data-level=(level.token()) {
                            td { (components::badge(level)) }
                            // `key` = 이 행의 식별 열. CSS가 최소 폭을 걸어 카드 여러 장의
                            // 설명 열이 같은 x에서 시작하게 한다(app.css `.dtable .key` 참고).
                            td class="mono key" { (item.label) }
                            td class="msg" { (registry.mask(&item.message)) }
                        }
                    }
                }
            }
        }
    }
}

/// stdout을 못 읽었을 때 붙는 진단 블록 — 종료 코드와 stderr 발췌를 함께 보여준다.
///
/// 종료 코드를 굳이 함께 싣는 이유: 파싱 실패 원인이 대개 "자식이 JSON을 찍기 전에 다른
/// 이유로 끝났다"이고, 그 이유는 종료 코드에 남는다(예: exit 2 → config 문제).
fn unreadable_notice(
    lang: Lang,
    verdict: Verdict,
    stderr: &str,
    registry: &SecretRegistry,
) -> Markup {
    let masked = registry.mask(stderr);
    components::notice(
        Level::Error,
        lang.sel("Child diagnostics", "자식 프로세스 진단"),
        html! {
            (components::meta_list(&[("verdict", verdict.headline(lang))]))
            @if masked.is_empty() {
                p class="muted" { (lang.sel("The child wrote nothing to stderr either.", "자식이 stderr에도 아무것도 쓰지 않았습니다.")) }
            } @else {
                pre class="logdump" { (masked) }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 실제 `doctor --json` 출력을 그대로 박아 둔 표본(프로파일 5개 · 항목 19개).
    ///
    /// 손으로 쓴 더미가 아니라 실제 바이너리 출력이다 — 필드 이름·유니코드 화살표·`db: null`
    /// 같은 실제 모양이 그대로 들어 있어야 파싱 회귀가 잡힌다.
    const SAMPLE_JSON: &str = r#"{"schema":1,"overall":"fail","profiles":[
        {"profile":"mongo-analytics","db":"mongodb","items":[
            {"label":"source","status":"ok","message":"resolved → mongodb"},
            {"label":"destination","status":"ok","message":"local: /srv/backups/mongo-analytics"},
            {"label":"encryption","status":"warn","message":"disabled — backups are stored in plaintext"},
            {"label":"engine","status":"ok","message":"mongodump (external tool required — native recommended)"}]},
        {"profile":"mysql-billing","db":"mysql","items":[
            {"label":"source","status":"ok","message":"resolved → mysql"},
            {"label":"encryption","status":"fail","message":"age but recipient_file does not exist: /etc/x-backup/missing-age.pub"}]},
        {"profile":"pg-staging","db":null,"items":[
            {"label":"source","status":"warn","message":"설정 오류: 시크릿 환경변수 'PG_STAGING_URI'가 설정되지 않았습니다"}]}]}"#;

    fn sample() -> Report {
        parse_report(SAMPLE_JSON.as_bytes()).expect("표본이 파싱되지 않는다")
    }

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    // -- 판정 -------------------------------------------------------------

    /// 0/4/3은 각각 다른 판정으로 접히고, **exit 4는 실패가 아니다**.
    #[test]
    fn exit_codes_map_to_distinct_verdicts() {
        assert_eq!(Verdict::from_exit(Some(0)), Verdict::Clean);
        assert_eq!(Verdict::from_exit(Some(4)), Verdict::Warned);
        assert_eq!(Verdict::from_exit(Some(3)), Verdict::Blocked);
        assert_eq!(Verdict::from_exit(Some(2)), Verdict::Misconfigured);
        assert_eq!(Verdict::from_exit(Some(5)), Verdict::Unexpected(Some(5)));
        assert_eq!(Verdict::from_exit(None), Verdict::Unexpected(None));
        assert_ne!(
            Verdict::Warned.level(),
            Level::Fail,
            "exit 4를 실패로 칠하면 안 된다 — 경고 동반 성공이다"
        );
    }

    /// 0/4/3이 화면에서 **서로 다르게 표현된다** — 레벨 토큰·배지 글자·헤드라인·디테일이
    /// 셋 다 겹치지 않아야 한다(색만 다르면 흑백 스크린샷에서 구분이 사라진다).
    #[test]
    fn zero_warn_fail_are_visually_distinct() {
        let cases = [Verdict::Clean, Verdict::Warned, Verdict::Blocked];
        for (i, a) in cases.iter().enumerate() {
            for b in &cases[i + 1..] {
                assert_ne!(a.level().token(), b.level().token(), "{a:?} vs {b:?} 레벨");
                assert_ne!(a.level().label(), b.level().label(), "{a:?} vs {b:?} 배지");
                for lang in [Lang::En, Lang::Ko] {
                    assert_ne!(
                        a.headline(lang),
                        b.headline(lang),
                        "{a:?} vs {b:?} 헤드라인({lang:?})"
                    );
                    assert_ne!(
                        a.detail(lang),
                        b.detail(lang),
                        "{a:?} vs {b:?} 디테일({lang:?})"
                    );
                }
            }
        }
    }

    /// exit 4 배너에는 "성공"이라는 사실이 글자로 적혀 있어야 한다 — 노란 배지만으로는
    /// 운영자가 "실패했나?"를 되묻는다.
    #[test]
    fn warned_detail_says_it_is_still_success() {
        assert!(
            Verdict::Warned
                .detail(Lang::En)
                .unwrap()
                .contains("success"),
            "en 디테일이 성공임을 말하지 않는다"
        );
        assert!(
            Verdict::Warned.detail(Lang::Ko).unwrap().contains("성공"),
            "ko 디테일이 성공임을 말하지 않는다"
        );
    }

    /// 렌더된 화면에서도 세 상태가 다른 `data-level`로 나간다(마크업 수준 회귀 고정).
    #[test]
    fn rendered_pages_carry_distinct_level_attributes() {
        let mut seen = Vec::new();
        for verdict in [Verdict::Clean, Verdict::Warned, Verdict::Blocked] {
            let outcome = Outcome::Reported {
                verdict,
                report: sample(),
            };
            let out = render(Lang::En, true, &outcome, &empty_registry()).into_string();
            let marker = format!(
                r#"class="verdict" data-level="{}""#,
                verdict.level().token()
            );
            assert!(
                out.contains(&marker),
                "{verdict:?} 배너 레벨 누락: {marker}"
            );
            seen.push(verdict.level().token());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 3, "세 판정이 같은 레벨로 뭉개졌다");
    }

    // -- 파싱 -------------------------------------------------------------

    /// 실제 출력 표본이 온전히 파싱된다.
    #[test]
    fn real_doctor_output_parses() {
        let report = sample();
        assert_eq!(report.schema, DOCTOR_JSON_SCHEMA);
        assert_eq!(report.overall, "fail");
        assert_eq!(report.profiles.len(), 3);
        assert_eq!(report.profiles[0].db.as_deref(), Some("mongodb"));
        assert_eq!(report.profiles[2].db, None, "db: null이 None으로 접혀야 함");
        assert_eq!(worst_level(&report.profiles[0].items), Level::Warn);
        assert_eq!(worst_level(&report.profiles[1].items), Level::Fail);
    }

    /// 상태 문자열 매핑 — 모르는 값은 초록이 아니라 [`Level::Error`]로 간다.
    #[test]
    fn unknown_status_does_not_become_ok() {
        assert_eq!(level_from_status("ok"), Level::Ok);
        assert_eq!(level_from_status("warn"), Level::Warn);
        assert_eq!(level_from_status("fail"), Level::Fail);
        for weird in ["", "OK", "skipped", "ok ", "unknown"] {
            assert_eq!(
                level_from_status(weird),
                Level::Error,
                "'{weird}'가 조용히 통과했다"
            );
        }
    }

    /// 병리적으로 깊은 stdout은 `serde_json`에 **닿기 전에** 거부된다 —
    /// [`crate::web::jsonguard`] 관문을 이 파서가 실제로 통과시키는지 고정한다.
    /// 깊이 판정 자체의 성질은 그 모듈의 테스트가 본다.
    #[test]
    fn pathologically_deep_stdout_is_refused_before_parsing() {
        let bomb = "[".repeat(50_000);
        assert_eq!(
            parse_report(bomb.as_bytes()).unwrap_err(),
            ReportError::TooDeep {
                found: 50_000,
                max: jsonguard::MAX_JSON_DEPTH,
            }
        );
        // 상한 안쪽은 통과 — 관문이 정상 보고서를 막지 않는다.
        let nested = format!(
            r#"{{"schema":{},"items":[{{"deep":{}{}}}]}}"#,
            DOCTOR_JSON_SCHEMA,
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(
            !matches!(
                parse_report(nested.as_bytes()),
                Err(ReportError::TooDeep { .. })
            ),
            "상한 안쪽 문서가 깊이를 이유로 거부됐다"
        );
    }

    /// 손상·빈·비-UTF8·스키마 불일치 입력이 전부 패닉 없이 오류로 접히고, 각각 다른
    /// 진단 문장을 낸다.
    #[test]
    fn broken_stdout_becomes_readable_errors() {
        assert_eq!(parse_report(b"").unwrap_err(), ReportError::Empty);
        assert_eq!(parse_report(b"   \n\t ").unwrap_err(), ReportError::Empty);
        // 0xFF는 어떤 UTF-8 시퀀스에도 나타날 수 없는 바이트다.
        assert_eq!(
            parse_report(&[b'{', 0xFF, b'}']).unwrap_err(),
            ReportError::NotUtf8
        );
        assert!(matches!(
            parse_report(b"{\"schema\":1,").unwrap_err(),
            ReportError::Malformed(_)
        ));
        assert!(matches!(
            parse_report(b"not json at all").unwrap_err(),
            ReportError::Malformed(_)
        ));
        // JSON이지만 최상위 필드가 없다.
        assert!(matches!(
            parse_report(b"{}").unwrap_err(),
            ReportError::Malformed(_)
        ));
        assert_eq!(
            parse_report(br#"{"schema":99,"overall":"ok","profiles":[]}"#).unwrap_err(),
            ReportError::SchemaMismatch {
                found: 99,
                expected: DOCTOR_JSON_SCHEMA
            }
        );
        // 스키마는 맞지만 profiles의 모양이 다르다.
        assert!(matches!(
            parse_report(br#"{"schema":1,"overall":"ok","profiles":"nope"}"#).unwrap_err(),
            ReportError::Malformed(_)
        ));

        // 네 종류의 설명이 서로 다르다 — 뭉개지면 운영자가 의심할 곳을 못 찾는다.
        let explains: Vec<String> = [
            ReportError::Empty,
            ReportError::NotUtf8,
            ReportError::Malformed("boom".into()),
            ReportError::SchemaMismatch {
                found: 2,
                expected: 1,
            },
        ]
        .iter()
        .map(|e| e.explain(Lang::En))
        .collect();
        for (i, a) in explains.iter().enumerate() {
            assert!(!a.is_empty(), "빈 설명");
            for b in &explains[i + 1..] {
                assert_ne!(a, b, "설명이 겹친다");
            }
        }
    }

    /// 깨진 출력이 들어와도 화면이 렌더되고(패닉 없음), 사람이 읽을 수 있는 문장이 나온다.
    #[test]
    fn unreadable_outcome_renders_human_error_page() {
        let outcome = Outcome::Unreadable {
            verdict: Verdict::Misconfigured,
            error: ReportError::Empty,
            stderr: "ERROR 사전 점검 실패: config 없음".to_string(),
        };
        let out = render(Lang::En, false, &outcome, &empty_registry()).into_string();
        assert!(out.contains(r#"data-level="error""#), "오류 레벨 누락");
        assert!(out.contains("produced no output"), "설명 문장 누락: {out}");
        assert!(out.contains("exit 2"), "종료 코드 근거 누락: {out}");
        assert!(out.contains("사전 점검 실패"), "stderr 발췌 누락");
    }

    /// 자식을 아예 못 돌린 경우도 렌더된다.
    #[test]
    fn unavailable_outcome_renders_human_error_page() {
        let outcome = Outcome::Unavailable {
            error: RunError::Timeout(DOCTOR_TIMEOUT),
        };
        let out = render(Lang::Ko, true, &outcome, &empty_registry()).into_string();
        assert!(out.contains(r#"data-level="error""#));
        assert!(out.contains("상한 시간"), "타임아웃 설명 누락: {out}");
        assert!(
            out.contains(&DOCTOR_TIMEOUT.as_secs().to_string()),
            "상한 값이 화면에 없다"
        );
    }

    /// 발췌는 상한을 넘지 않고, 멀티바이트 경계를 깨지 않는다.
    #[test]
    fn excerpt_truncates_on_char_boundary() {
        let long: String = "가".repeat(EXCERPT_CHARS * 2);
        let cut = excerpt(&long);
        assert_eq!(cut.chars().count(), EXCERPT_CHARS + 1, "말줄임표 포함 길이");
        assert!(cut.ends_with('…'));
        assert_eq!(excerpt("  hi  "), "hi", "앞뒤 공백은 잘라낸다");
    }

    // -- 렌더 안전성 -----------------------------------------------------

    /// 적대적 입력(HTML 태그·제어문자·초장문)이 전부 이스케이프되고 화면을 깨지 않는다.
    #[test]
    fn hostile_child_output_is_escaped() {
        let hostile = r#"<script>alert('x')</script>"#;
        let very_long = "A".repeat(20_000);
        let json = serde_json::json!({
            "schema": DOCTOR_JSON_SCHEMA,
            "overall": hostile,
            "profiles": [{
                "profile": hostile,
                "db": hostile,
                "items": [
                    {"label": hostile, "status": hostile, "message": hostile},
                    // 제어문자(NUL·BEL·이스케이프)와 초장문.
                    {"label": "ctl", "status": "ok", "message": "\u{0000}\u{0007}\u{001b}[31mred\u{001b}[0m"},
                    {"label": "long", "status": "warn", "message": very_long},
                ],
            }],
        })
        .to_string();
        let report = parse_report(json.as_bytes()).expect("적대적 값도 파싱은 되어야 함");
        let outcome = Outcome::Reported {
            verdict: Verdict::Blocked,
            report,
        };
        let out = render(Lang::En, true, &outcome, &empty_registry()).into_string();
        assert!(!out.contains("<script>"), "스크립트 태그가 살아 있다");
        assert!(out.contains("&lt;script&gt;"), "이스케이프 형태가 아니다");
        // 알 수 없는 status는 error 레벨로 그려진다.
        assert!(out.contains(r#"data-level="error""#), "미지 상태 처리 누락");
        // 초장문도 그대로 실린다(잘라내는 건 stderr 발췌뿐 — 점검 메시지는 정보다).
        assert!(out.contains(&very_long), "긴 메시지가 유실됐다");
    }

    /// 등록된 시크릿이 점검 메시지에 섞여 있으면 마스킹된다(2차 방어).
    #[test]
    fn registered_secrets_are_masked_in_messages() {
        const FAKE: &str = "NOT-A-REAL-SECRET-doctorview-9f2b7c41";
        let json = serde_json::json!({
            "schema": DOCTOR_JSON_SCHEMA,
            "overall": "warn",
            "profiles": [{
                "profile": "p", "db": "postgresql",
                "items": [{"label": "source", "status": "warn", "message": format!("uri=postgres://u:{FAKE}@h/db")}],
            }],
        })
        .to_string();
        let outcome = Outcome::Reported {
            verdict: Verdict::Warned,
            report: parse_report(json.as_bytes()).unwrap(),
        };
        let mut registry = SecretRegistry::new();
        assert!(registry.register(FAKE), "표본 시크릿 등록 실패");
        let out = render(Lang::En, true, &outcome, &registry).into_string();
        assert!(!out.contains(FAKE), "시크릿 원문이 화면에 남았다");
        assert!(out.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    /// 렌더 결과에 서버 내부 경로가 새지 않는다 — `render`는 경로를 받지도 않으므로
    /// 구조적으로 불가능하다는 것을 시그니처와 함께 고정한다(t5의
    /// `markup_does_not_leak_server_paths`와 같은 취지).
    #[test]
    fn render_cannot_leak_server_paths() {
        let outcome = Outcome::Reported {
            verdict: Verdict::Clean,
            report: sample(),
        };
        for present in [true, false] {
            let out = render(Lang::En, present, &outcome, &empty_registry()).into_string();
            assert!(!out.contains("/nonexistent/state"), "state 경로 노출");
            assert!(!out.contains(".local/state"), "state 경로 노출");
            assert!(!out.contains("doctor-demo.toml"), "config 경로 노출");
            // config는 존재 여부만 말한다.
            assert!(out.contains(if present { "wired" } else { "not wired" }));
        }
    }

    /// 프로파일 카드가 밀도 있는 요약과 최악 상태 배지를 함께 낸다.
    #[test]
    fn profile_panel_summarizes_counts_and_worst_level() {
        let report = sample();
        let out = profile_panel(Lang::En, &report.profiles[0], &empty_registry()).into_string();
        assert!(
            out.contains("4 checks · 3 ok · 1 warn · 0 fail"),
            "요약 누락: {out}"
        );
        assert!(out.contains(r#"data-level="warn""#), "최악 상태 배지 누락");
        assert!(out.contains("mongodb"), "엔진 태그 누락");
    }

    /// 항목이 없는 프로파일도 표가 깨지지 않고 안내 문장이 나온다.
    #[test]
    fn empty_item_list_renders_placeholder_row() {
        let out = items_table(Lang::En, &[], &empty_registry()).into_string();
        assert!(out.contains("No checks were reported"), "안내 문장 누락");
        assert!(out.contains(r#"colspan="3""#), "표 구조가 깨졌다");
    }

    // -- 자식 실행 -------------------------------------------------------

    use crate::web::job::{JobRunner, JobSecrets};

    /// `/usr/bin/env`처럼 자식 환경을 그대로 찍어 주는 프로그램으로 러너를 만든다.
    fn runner_with(exe: &str, secrets: JobSecrets) -> JobRunner {
        JobRunner::with_exe(PathBuf::from(exe), None, Lang::En, secrets)
    }

    /// 점검 실행 파일은 **항상 자기 자신**이다 — PATH를 타지 않는다.
    #[test]
    fn doctor_exe_is_current_exe() {
        let exe = doctor_exe().expect("current_exe 실패");
        assert_eq!(
            exe,
            std::env::current_exe().unwrap(),
            "current_exe()가 아닌 경로를 쓰고 있다"
        );
        assert!(exe.is_absolute(), "절대 경로가 아니다: {}", exe.display());
    }

    /// 이 화면이 만드는 명세는 `doctor --json` 하나뿐이다 — `--config`/`--lang`을 직접
    /// 붙이지 않는다(붙이면 점검이 본 config와 잡이 볼 config가 갈라질 수 있다).
    #[test]
    fn doctor_spec_is_plain_doctor_json() {
        let spec = doctor_spec(Lang::En);
        assert_eq!(spec.to_argv(), vec!["doctor", "--json"]);
        assert!(
            !spec.is_destructive(),
            "doctor가 파괴적으로 분류되면 감사 게이트 없이 spawn()으로 띄울 수 없다"
        );
        assert!(spec.profile().is_none(), "점검은 전 프로파일을 본다");
    }

    /// **자식 env가 잡과 같다** — H2·H3의 회귀 테스트이며 이 파일 수정의 핵심이다.
    ///
    /// 두 방향을 한 번에 확인한다:
    /// - `XB_*__*` config 오버라이드가 자식에게 **가지 않는다**(옛 블랙리스트는 상속했다 →
    ///   화면은 초록인데 잡은 exit 2로 끊기는 거짓 초록).
    /// - 기동 시점에 서버 환경에서 제거된 시크릿이 자식 env에 **다시 주입된다**(옛 코드는
    ///   제거된 env를 상속해 정상 설정을 상시 오진했다).
    #[cfg(unix)]
    #[tokio::test]
    async fn doctor_child_env_matches_the_job_runner() {
        // env 가드는 spawn까지만 필요하다(자식 env는 spawn 시점에 확정된다) — `.await`
        // 너머로 들고 가면 clippy(`await_holding_lock`)이고, 실제로도 이 테스트가 자식을
        // 기다리는 동안 다른 파일의 env 테스트를 막을 이유가 없다.
        let running = {
            let _guard = crate::web::job::env_guard();
            std::env::set_var("XB_SOURCE__URI", "mongodb://leaked-override/db");

            let mut secrets = JobSecrets::new();
            secrets.insert_value("XB_TEST_DOCTOR_URI", "mongodb://u:doctor-secret-value@h/db");
            let runner = runner_with("/usr/bin/env", secrets);
            let running = runner.spawn_raw(Vec::new()).expect("env 실행 실패");

            std::env::remove_var("XB_SOURCE__URI");
            running
        };
        let run = await_with_limit(running, DOCTOR_TIMEOUT)
            .await
            .expect("자식 실행 실패");
        let override_name = "XB_SOURCE__URI";

        assert!(
            !run.stdout.contains(override_name),
            "config 오버라이드가 점검 자식에게 상속됐다(잡은 이것을 받지 않는다): {}",
            run.stdout
        );
        assert!(
            run.stdout.contains("XB_TEST_DOCTOR_URI"),
            "시크릿이 점검 자식에게 주입되지 않았다 — 정상 설정을 오진한다: {}",
            run.stdout
        );
        assert!(
            !run.stdout.contains(crate::web::auth::ENV_WEB_TOKEN),
            "세션 토큰이 자식 env에 남는다"
        );
    }

    /// 존재하지 않는 실행 파일은 spawn 실패로 접힌다(패닉·무한 대기 없음).
    #[tokio::test]
    async fn missing_executable_is_spawn_error() {
        let runner = runner_with(
            "/nonexistent/x-backup-does-not-exist-9f2b",
            JobSecrets::new(),
        );
        let err = run_doctor(&runner, DOCTOR_TIMEOUT, Lang::En)
            .await
            .expect_err("없는 파일이 성공하면 안 됨");
        assert!(matches!(err, RunError::Spawn(_)), "{err:?}");
        assert!(!err.explain(Lang::En).is_empty());
    }

    /// 오래 걸리는 자식은 상한에서 끊긴다 — 무한 대기가 없다는 것을 실제로 확인한다.
    ///
    /// 상한을 짧게(200ms) 주고 5초 자는 자식을 붙인다. 실제 [`DOCTOR_TIMEOUT`]을 쓰면
    /// 테스트가 20초 걸리므로 값이 아니라 **메커니즘**을 검증한다. 러너는 `kill_on_drop`을
    /// 켜지 않으므로 이 경로가 실제로 시그널을 보내는지가 관건이다.
    #[cfg(unix)]
    #[tokio::test]
    async fn slow_child_hits_the_timeout() {
        let runner = runner_with("/bin/sh", JobSecrets::new());
        let running = runner
            .spawn_raw(vec!["-c".to_string(), "sleep 5".to_string()])
            .expect("sh 실행 실패");
        let pid = running.pid().expect("pid를 알 수 없다");
        let limit = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let err = await_with_limit(running, limit)
            .await
            .expect_err("타임아웃이 걸리지 않았다");
        assert_eq!(err, RunError::Timeout(limit));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "상한을 기다리지 않고 자식이 끝날 때까지 매달렸다: {:?}",
            started.elapsed()
        );

        // 자식이 실제로 죽었는지 — 시그널 0은 "존재 확인"이다. 수거(orphan 큐)까지는
        // 시간이 걸릴 수 있으므로 잠깐 기다려 준다.
        let mut alive = true;
        for _ in 0..50 {
            // SAFETY: 시그널 0은 전송하지 않고 대상 존재/권한만 검사한다.
            if unsafe { libc::kill(pid as libc::pid_t, 0) } != 0 {
                alive = false;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!alive, "상한을 넘긴 자식이 계속 돌고 있다(pid {pid})");
    }

    /// 정상 종료한 자식의 stdout·종료 코드를 그대로 모은다(파이프 연결 확인).
    #[cfg(unix)]
    #[tokio::test]
    async fn child_output_and_exit_code_are_collected() {
        let runner = runner_with("/bin/sh", JobSecrets::new());
        let running = runner
            .spawn_raw(vec!["-c".to_string(), "printf 'hello'; exit 4".to_string()])
            .expect("sh 실행 실패");
        let run = await_with_limit(running, DOCTOR_TIMEOUT)
            .await
            .expect("자식 실행 실패");
        assert_eq!(run.exit, Some(4));
        assert_eq!(run.stdout, "hello");
        assert_eq!(Verdict::from_exit(run.exit), Verdict::Warned);
    }

    // -- 마스킹 레지스트리 -------------------------------------------------

    /// **레지스트리가 잡 러너의 것이다** — H4의 회귀 테스트.
    ///
    /// 옛 코드는 `XB_AES_KEY_HEX`를 env에서 다시 읽었는데 그 값은 기동 시점에 이미
    /// 제거되므로 등록이 **항상 0건**이었다(리뷰가 destination 경로에 키 hex를 심어 원문
    /// 노출을 실측했다). 지금은 러너가 자식에게 실제로 주입하는 값들을 그대로 쓴다.
    #[test]
    fn request_registry_masks_values_the_runner_injects() {
        const FAKE_KEY: &str = "NOT-A-REAL-SECRET-aaaabbbbccccdddd1111";
        let mut secrets = JobSecrets::new();
        secrets.insert_value(crate::pipeline::stage::ENV_AES_KEY_HEX, FAKE_KEY);

        let mut ctx = ServeConfig::for_test();
        ctx.jobs =
            Arc::new(JobRunner::new(None, Lang::En, secrets).expect("테스트 러너 생성 실패"));

        let registry = request_registry(&ctx, None);
        let masked = registry.mask(&format!("local: /tmp/{FAKE_KEY}"));
        assert!(
            !masked.contains(FAKE_KEY),
            "러너가 주입한 시크릿이 화면에 원문으로 남는다: {masked}"
        );
        assert!(masked.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    /// 세션 토큰은 출처(env/파일)와 무관하게 [`AuthState`]에서 받아 등록한다.
    #[test]
    fn request_registry_masks_the_session_token_from_any_source() {
        const TOKEN: &str = "doctor-view-session-token-9f2b7c41";
        let ctx = ServeConfig::for_test();
        let auth = AuthState::for_test(TOKEN);

        let without = request_registry(&ctx, None);
        assert!(
            without.mask(TOKEN).contains(TOKEN),
            "이 대조군이 통과해야 아래 단정이 의미가 있다"
        );

        let with = request_registry(&ctx, Some(&auth));
        assert!(
            !with.mask(&format!("boom: {TOKEN}")).contains(TOKEN),
            "세션 토큰이 마스킹되지 않는다"
        );
    }
}
