//! `GET/POST /verify` — 백업 무결성 검증 화면. `GET /verify/{job_id}`(결과) 함께 배선한다.
//!
//! ## 세 모드, 그런데 웹은 이 중 하나를 재구현하지 않는다
//! `verify`는 세 층위로 동작한다(`src/cli/handlers/verify.rs` 헤더): **구조**(기본, 키
//! 불필요) · **`--deep`**(복호화·압축해제까지 디코드) · **`--chain`**(base+증분 연속성).
//! 이 파일은 그 판정을 한 줄도 다시 하지 않는다 — 자식 `verify --id <ID> [--deep]
//! [--chain] --json`을 [`JobSpec`]으로 조립해 [`crate::web::job::JobRunner`]에 넘기고,
//! 그 자식이 stdout에 낸 JSON 문서를 구조체로 옮겨 담을 뿐이다([`VerifyDoc`]).
//!
//! ## 잡으로 돌릴까, 요청 안에서 끝낼까 — **잡으로, 셋 다 통일한다**
//! `--deep`은 백업 전체를 복호화·압축해제하므로 수 분~수십 분 걸릴 수 있다(`doctor.rs`의
//! 20초 상한 같은 값을 고를 수 없다). 구조 검증만이면 수십~수백 ms로 끝난다. 그런데도
//! 세 모드를 전부 잡으로 통일한 이유:
//!
//! 1. **두 경로를 만들면 화면이 두 벌이 된다.** "구조는 요청 안에서, deep/chain은 잡으로"
//!    나누면 폼 제출 처리·결과 렌더링·오류 화면이 전부 두 가지 모양이 되고, 사용자는
//!    같은 화면인데 어떤 모드를 골랐느냐에 따라 다르게 반응하는 콘솔을 만난다.
//! 2. **`ctx.jobs.spawn`이 이미 락 상속·env 화이트리스트·시크릿 주입을 전부 해 준다.**
//!    요청 안에서 끝내려면 이 태스크가 그 인프라를 다시 조립해야 한다.
//! 3. **짧은 검증도 이력에 남는 것은 비용이지 결함이 아니다.** "누가 언제 무엇을
//!    검증했는가"는 감사 대상이고(리더 지시서), 잡 이력이 바로 그 기록이다. 구조 검증이
//!    자주 눌리는 화면이 되면 이력이 촘촘해지지만, 그 대가로 얻는 단순함(화면 하나·경로
//!    하나)이 이 콘솔의 다른 화면들과 일관된다.
//!
//! ## `spawn_tracked_job`을 그대로 가져다 쓰지 않은 이유 — stdout이 저장되지 않는다
//! [`crate::web::routes::backup::spawn_tracked_job`]을 먼저 살펴봤다. 그 함수는
//! [`crate::web::job::stream::relay_stdout`]로 stdout을 SSE로만 흘리고, **stdout을 잡
//! 로그에 적재하지 않는다**([`crate::web::job::stream::relay_stderr`]에만 `JobLogSink`를
//! 물린다). `backup`/`restore`는 진행률(progress) 이벤트가 stdout에 오고 최종 결과 요약은
//! SSE `done` 이벤트에 실려 그 순간 화면에 있는 사람만 본다 — 새로고침하거나 나중에 다시
//! 들어오면 사라진다. `pipeline/verify.rs`를 확인한 결과 verify는 진행률을 전혀 찍지
//! 않는다(진행률 리포터를 부르지 않는다) — stdout은 오직 마지막 한 줄, `--json` 결과
//! 문서뿐이다. 그런데 이 화면의 존재 이유가 정확히 그 결과 문서다. `spawn_tracked_job`을
//! 그대로 쓰면 그 문서가 세션이 끝나는 순간 사라지므로, 이 파일은 **자체적으로**(더 얇게)
//! 잡을 spawn한다: SSE 허브를 만들지 않고, [`RunningJob::wait_with_output`](자식이 짧게
//! 끝나는 잡을 위해 doctor.rs가 이미 쓰는 편의 함수)로 stdout·stderr **전체**를 모아
//! **둘 다** 잡 로그에 적재한다. 그래야 `GET /verify/{job_id}`가 몇 시간 뒤에 열려도 같은
//! 결과를 그대로 보여준다.
//!
//! (참고로 이건 이 브랜치의 잡 파이프라인 전반에 있는 잠재적 한계다 — `backup`의 최종
//! 요약도 SSE를 놓치면 사라진다. 고치는 것은 이 태스크 범위 밖이라 여기서는 verify만
//! 스스로 해결한다.)
//!
//! ## exit 4를 verify 문맥에서 어떻게 읽어야 하는가 — **가장 중요한 함정**
//! `job::exit::present()`는 **라이브 [`JobOutcome`] 값**을 받아야 문구를 만든다(t13 헤더:
//! `Signaled(i32)`의 시그널 번호가 라벨 문자열만으로는 복원되지 않으므로 label → Outcome
//! 역변환을 만들지 않았다). 이 화면의 `GET /verify/{job_id}`는 **저장소에서 다시 읽은
//! 문자열 라벨**([`crate::web::state::jobs::JobSummary::outcome`])만 갖고 있고, 잡을 막
//! 끝낸 그 요청도 아니다 — 그래서 `present()`를 부를 수 없다. `crate::web::view::jobs`가
//! 이미 같은 이유로 자기만의 `outcome_headline`/`outcome_detail`을 갖고 있고
//! ([`crate::web::view::jobs`] 헤더), 이 화면도 같은 결정을 내린다: **레벨(색)은
//! [`job_exit::level_for_label`] 하나로 통일**(중복 매핑을 만들지 않는다)하고, **문구는
//! verify 문맥에 맞게 새로 쓴다**([`crate::web::view::verify::outcome_detail`]).
//!
//! 그 문구가 반드시 말해야 하는 것: **verify의 exit 4는 backup의 exit 4(oplog gap → 풀
//! 승격)와 뜻이 다르다.** verify에서 exit 4는 "검증이 끝까지 실행됐고, 이 백업에서 표시할
//! 문제(불완전한 manifest, 끊긴 체인 등)를 찾았다"는 뜻이다 — 둘 다 "경고 동반 성공"이라는
//! 공통점은 있지만 *무엇에 대한* 경고인지가 다르므로, 같은 배지 색만 보고 두 화면을 넘나드는
//! 운영자가 헷갈리지 않게 화면 문구가 그 차이를 명시한다.
//!
//! exit 5(락 충돌)는 [`JobOutcome::retryable`]과 같은 판정을 라벨 문자열로 다시 쓴다
//! (`outcome_label == "lock-conflict"`) — 그 매핑은 [`JobOutcome::label`]/`retryable` 정의와
//! 1:1이므로 갈라질 수 없다(그래도 verify가 실제로 락을 잡지는 않는다 — 아래 "verify가
//! 락을 잡지 않는다" 참고. 방어적으로만 다룬다).
//!
//! ## `--deep` 키 미설정 — 이유는 명확히, 경로·값은 노출하지 않는다
//! `verify --deep`이 키 없이 실행되면 CLI는 exit 1로 거부하고
//! `"...— verify --deep은 개인키 보유 호스트에서 실행하세요(§8.5 키 격리)"`를 stderr에
//! 남긴다(`cli/handlers/verify.rs::run`). 그 안쪽 원인 메시지(`pipeline/stage.rs::
//! resolve_decrypt_key`)는 env가 **설정되지 않았을 때만** 나오므로 변수 *이름*
//! (`XB_AGE_IDENTITY_FILE`/`XB_AES_KEY_HEX`)만 담고 경로·값은 절대 담지 않는다 — 아직 읽지도
//! 못했으니 보여줄 경로가 없다. 그래서 [`deep_key_missing`]은 그 이름이 stderr에 나타나는지
//! 로 "키 미설정" 상태를 감지하고, 화면은 원문 stderr를 그대로 보여주는 대신 **경로·값이
//! 전혀 없는 큐레이션된 안내문**을 낸다([`crate::web::view::verify::deep_key_missing_notice`]).
//! (일반적인 실패는 반대로 마스킹을 거친 stderr 발췌를 그대로 보여준다 — `doctor.rs`와 같은
//! 판단이다. 경로는 `mask.rs`의 정책상 시크릿이 아니라 진단 정보이므로 보통은 가려지지
//! 않는다. 이 화면이 이 한 경로에서만 예외를 두는 이유는 리더 지시서가 명시적으로 요구했기
//! 때문이다.)
//!
//! ## verify가 락을 잡지 않는다 — 그런데 왜 exit 5를 다루는가
//! `cli/handlers/verify.rs`를 읽어 보면 verify는 파일 락을 잡지 않는다(읽기 전용 검증이라
//! 상호 배제가 필요 없다). 그래서 실전에서 exit 5(락 충돌)는 나오지 않는다. 그래도 이
//! 화면은 [`job_exit::level_for_label`]의 아홉 라벨 전부를 무차별하게 받아 넘긴다 —
//! 어휘를 좁혀서 얻는 이득이 없고("verify는 5가 안 나온다"를 여기서 강제하면 CLI가 나중에
//! 검증에도 잠금을 도입했을 때 이 화면만 조용히 뒤처진다), 아홉 라벨을 전부 다루는 값싼
//! 방어이므로 그대로 둔다.
//!
//! ## 이 화면이 손대지 않는 것
//! - `crate::engine`·`crate::storage`·`crypto`·`crate::manifest` 직접 호출 없음(최상위
//!   불변식). 체크섬·체인·디코드는 전부 자식이 계산한다.
//! - 프로파일 선택 폼 필드가 없다 — `VerifyArgs`(`src/cli/args.rs`)에는 애초에 `--profile`이
//!   없다(현 CLI 표면). verify는 항상 config의 `default_profile`(없으면 `"default"`)이
//!   가리키는 destination을 본다. 화면은 그 사실을 숨기지 않고 대상 프로파일을
//!   정보성으로만 보여준다([`resolve_target_profile`]) — 고를 수 있는 것처럼 꾸미면 운영자가
//!   실제로 검증되는 프로파일과 다른 프로파일을 골랐다고 착각한다.
//! - "최신 백업 자동 채움"은 넣지 않았다. 시도하려면 `list` 잡을 하나 더 spawn해야 하는데,
//!   GET 요청 하나의 수명 안에서 안전하게 취소할 방법이 없다(타임아웃으로 퓨처를 드롭하면
//!   `RunningJob`의 `Child`가 `kill_on_drop` 없이 그대로 남아 이력에 없는 고아 프로세스가
//!   된다 — `job::runner` 헤더가 명시적으로 경계하는 바로 그 상황). 카탈로그 화면(t17)의
//!   링크(`?id=`)로 채우는 경로만 지원한다.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use maud::Markup;
use serde::Deserialize;

use crate::error::XBackupError;
use crate::web::audit::{AuditEvent, AuditOutcome};
use crate::web::job::{lifecycle, JobCommand, JobFlag, JobOutcome, JobSpec, RunningJob};
use crate::web::jsonguard;
use crate::web::mask::SecretRegistry;
use crate::web::state::jobs::{self, JobId, JobStart, JobStore, LogStream};
use crate::web::view::{layout, verify as view};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수
// ---------------------------------------------------------------------------

/// 폼(GET)·제출(POST) 경로.
pub const VERIFY_PATH: &str = "/verify";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const VERIFY_TITLE: &str = "Verify";
/// 결과 화면의 **라우터 패턴**(axum 0.8 `{name}` 문법).
pub const VERIFY_RESULT_ROUTE: &str = "/verify/{job_id}";

/// 감사 로그 `actor` 필드 값 — `routes::backup::AUDIT_ACTOR`와 같은 값·같은 이유지만,
/// 그 상수가 `pub`이 아니고 병행 편집 중인 파일에 결합하지 않기 위해 여기 독립적으로 둔다
/// (`routes::backup` 모듈 헤더 "이 화면이 손대지 않는 것"과 같은 판단).
const AUDIT_ACTOR: &str = "web";

/// `verify --json`이 낼 스키마 버전의 웹 쪽 사본.
///
/// `cli/handlers/verify.rs::VERIFY_JSON_SCHEMA`는 `pub`이 아니다(doctor.rs의
/// `DOCTOR_JSON_SCHEMA`와 달리). 그 파일은 이 태스크의 소유가 아니므로 가시성을 바꾸지
/// 않는다 — 대신 값을 여기 복제해 둔다. 두 값이 갈라지면(`verify` 쪽에서 스키마를
/// 올렸는데 여기를 안 고치면) [`parse_verify_doc`]이 `SchemaMismatch`로 **명확히** 실패하므로
/// (조용한 오파싱이 아니다), 드리프트가 나면 곧바로 화면에 드러난다.
const EXPECTED_VERIFY_SCHEMA: u32 = 1;

/// 폼 필드 이름 — GET(`?id=`) 프리필과 POST 제출이 공유한다.
const FIELD_ID: &str = "id";
const FIELD_DEEP: &str = "deep";
const FIELD_CHAIN: &str = "chain";

/// config에 `default_profile`이 없을 때의 표시 이름 — `cli/handlers/verify.rs::
/// default_profile_name`과 같은 기본값이다(그 함수도 `pub`이 아니라 재사용할 수 없다).
const DEFAULT_PROFILE_DISPLAY: &str = "default";

/// 결과 화면 링크.
pub fn verify_result_href(id: &JobId) -> String {
    format!("{VERIFY_PATH}/{id}")
}

// ---------------------------------------------------------------------------
// `verify --json` 문서 — 자식 stdout을 옮겨 담는 값 타입 (순수)
// ---------------------------------------------------------------------------

/// `verify --json`의 최상위 문서. 필드 이름은 `cli/handlers/verify.rs::build_json`과
/// 1:1로 맞춘다 — 한쪽만 바뀌면 [`ParseProblem::Malformed`]로 즉시 드러난다.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct VerifyDoc {
    /// 스키마 버전. [`parse_verify_doc`]이 [`EXPECTED_VERIFY_SCHEMA`]와 먼저 대조한다.
    pub schema: u32,
    /// 검증한 백업 ID.
    pub backup_id: String,
    /// 사이드카 체크섬으로 manifest 자체 정합이 확인됐는지.
    pub manifest_sidecar_ok: bool,
    /// data.bin 재계산 sha256이 manifest.checksum_sha256과 일치하는지.
    pub data_checksum_ok: bool,
    /// `--deep` 수행 시 디코드가 끝까지 성공했는지. `--deep`을 안 걸었으면 `null`(`None`).
    pub deep_decode_ok: Option<bool>,
    /// 빈 증분 슬라이스(oplog_count==0)로 data 부재가 정상 처리된 경우 `true`.
    pub empty_slice: bool,
    /// incomplete manifest 등 경고. 비어 있으면 무경고.
    pub warnings: Vec<String>,
    /// 구조(+심층) 검증이 전부 통과했는지.
    pub ok: bool,
    /// `--chain`을 걸었을 때만 채워진다.
    pub chain: Option<ChainDoc>,
}

/// `chain` 검증 결과. `cli/handlers/verify.rs::build_json`의 `chain` 객체와 1:1.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ChainDoc {
    pub base_id: String,
    pub incremental_ids: Vec<String>,
    pub continuous: bool,
    pub breaks: Vec<String>,
    pub warnings: Vec<String>,
}

/// stdout을 [`VerifyDoc`]으로 접지 못한 이유(`routes::doctor::ReportError`와 같은 설계).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseProblem {
    /// stdout이 비었다. **verify에서는 이것이 대개 정상이다** — exit 0/4가 아닌 모든
    /// 결과는 결과 문서를 아예 찍지 않는다(`cli/handlers/verify.rs::run` 참고). 그래서
    /// 화면은 이 값을 "무언가 손상됐다"로 알리지 않고, 성공/경고 결과에서만 이례로 다룬다.
    Empty,
    /// JSON이 아니거나 기대한 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 이 빌드가 아는 값과 다르다.
    SchemaMismatch { found: u64, expected: u32 },
    /// 중첩이 너무 깊어 **파싱을 시도하지 않았다**([`crate::web::jsonguard`]).
    TooDeep { found: usize, max: usize },
}

impl ParseProblem {
    /// 오류 화면에 그대로 들어가는 설명 문장.
    pub fn explain(&self, lang: crate::i18n::Lang) -> String {
        match self {
            ParseProblem::Empty => lang
                .sel(
                    "verify produced no output.",
                    "verify가 아무 출력도 내지 않았습니다.",
                )
                .to_string(),
            ParseProblem::Malformed(detail) => format!(
                "{} {}",
                lang.sel(
                    "verify output could not be parsed as the expected JSON:",
                    "verify 출력을 기대한 JSON으로 해석할 수 없습니다:",
                ),
                detail
            ),
            ParseProblem::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} \u{2260} {expected})",
                lang.sel(
                    "verify reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "verify가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            ParseProblem::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "verify output is nested more deeply than this console parses. The verification itself still ran — read the exit code.",
                    "verify 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다. 검증 자체는 수행됐습니다 — 종료 코드를 보세요.",
                )
            ),
        }
    }
}

/// 자식 stdout(이미 마스킹된 텍스트)을 [`VerifyDoc`]으로 접는다.
///
/// **스키마 버전을 본문 역직렬화보다 먼저 본다** — `doctor.rs::parse_report`와 같은
/// 근거다: 나중에 스키마가 바뀌었을 때 "필드가 이상하다"가 아니라 "버전이 다르다"로
/// 진단되게 한다. 비-UTF8 입력은 이 함수에 도달하기 전에 이미
/// [`RunningJob::wait_with_output`]이 lossy 변환을 마쳤으므로(대체 문자로 치환), 이 함수는
/// 항상 유효한 `&str`만 받고 그 문자열이 JSON으로 안 접히면 그냥 `Malformed`로 떨어진다 —
/// 패닉하지 않는다.
pub fn parse_verify_doc(stdout: &str) -> std::result::Result<VerifyDoc, ParseProblem> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(ParseProblem::Empty);
    }
    jsonguard::check_depth(trimmed).map_err(|d| ParseProblem::TooDeep {
        found: d.found,
        max: d.max,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| ParseProblem::Malformed(e.to_string()))?;
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(found) if found == u64::from(EXPECTED_VERIFY_SCHEMA) => {}
        Some(found) => {
            return Err(ParseProblem::SchemaMismatch {
                found,
                expected: EXPECTED_VERIFY_SCHEMA,
            })
        }
        None => {
            return Err(ParseProblem::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }
    serde_json::from_value(value).map_err(|e| ParseProblem::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------
// 저장된 argv에서 값을 읽는 헬퍼 — view가 재사용한다
// ---------------------------------------------------------------------------

/// 마스킹된 argv에 값 없는 플래그(`--deep`/`--chain`)가 있는지.
pub fn args_have_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

/// 마스킹된 argv에서 `<플래그> <값>` 쌍의 값을 찾는다(`--id`).
///
/// 이 어휘에서 `--id`의 값은 시크릿이 될 수 없으므로([`JobSpec`] 헤더 "웹은 접속 URI를
/// 인자로 받지 않는다") 항상 원문이다.
pub fn arg_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.iter()
        .position(|a| a == flag)
        .and_then(|i| args.get(i + 1))
        .map(String::as_str)
}

/// `--deep`이 키 미설정으로 거부됐는지 감지한다(모듈 헤더 "`--deep` 키 미설정" 참고).
///
/// stderr(마스킹을 거친 것)에 키 env 변수 *이름*이 나타나는지만 본다 — 그 문구는 env가
/// 설정되지 않았을 때만 나오므로 경로·값이 실려 있을 수 없다. `deep`이 아니거나 결과가
/// `failed`가 아니면 애초에 이 경로가 아니므로 즉시 `false`다.
pub fn deep_key_missing(deep_requested: bool, outcome_label: &str, stderr_masked: &str) -> bool {
    deep_requested
        && outcome_label == "failed"
        && (stderr_masked.contains(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE)
            || stderr_masked.contains(crate::pipeline::stage::ENV_AES_KEY_HEX))
}

// ---------------------------------------------------------------------------
// 대상 프로파일 표시 — config를 직접 읽는다(`routes::backup::load_profile_names`와 같은
// 이유로 CLI 내부를 재사용하지 않는다 — 그 함수도 `pub`이 아니다)
// ---------------------------------------------------------------------------

fn resolve_target_profile(ctx: &ServeConfig) -> String {
    let Some(path) = &ctx.config_path else {
        return DEFAULT_PROFILE_DISPLAY.to_string();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return DEFAULT_PROFILE_DISPLAY.to_string();
    };
    let Ok(config) = crate::config::file::Config::from_toml_str(&text) else {
        return DEFAULT_PROFILE_DISPLAY.to_string();
    };
    config
        .default_profile
        .unwrap_or_else(|| DEFAULT_PROFILE_DISPLAY.to_string())
}

/// 이 요청에서 쓸 시크릿 레지스트리 — `routes::jobs::request_registry`와 같은 판단.
fn request_registry(ctx: &ServeConfig) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    crate::web::mask::register_from_env_names(&mut registry, [crate::web::auth::ENV_WEB_TOKEN]);
    registry
}

// ---------------------------------------------------------------------------
// 폼 본문 파싱 — GET `?id=`와 POST 바디 둘 다 `key=value&...` 형태라 하나로 쓴다
// (`routes::backup`·`routes::config`와 같은 이유로 자체 파서를 둔다 — 그쪽 타입은 비공개다)
// ---------------------------------------------------------------------------

struct FormBody {
    pairs: Vec<(String, String)>,
}

impl FormBody {
    fn parse(body: &str) -> Self {
        let mut pairs = Vec::new();
        for pair in body.as_bytes().split(|&b| b == b'&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(eq) => {
                    let (k, v) = pair.split_at(eq);
                    (percent_decode(k), percent_decode(&v[1..]))
                }
                None => (percent_decode(pair), String::new()),
            };
            pairs.push((name, value));
        }
        Self { pairs }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
    }

    fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }
}

fn percent_decode(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < input.len()
                && input[i + 1].is_ascii_hexdigit()
                && input[i + 2].is_ascii_hexdigit() =>
            {
                out.push((hex_val(input[i + 1]) << 4) | hex_val(input[i + 2]));
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

fn query_param(query: Option<&str>, name: &str) -> Option<String> {
    let q = query?;
    FormBody::parse(q).get(name).map(str::to_string)
}

// ---------------------------------------------------------------------------
// 잡 spawn — 모듈 헤더 "`spawn_tracked_job`을 그대로 가져다 쓰지 않은 이유" 참고
// ---------------------------------------------------------------------------

enum SubmitError {
    /// 검증 실패 — 사용자가 고칠 수 있는 입력 문제.
    Validation(String),
    /// 그 외(감사 로그 기록 실패, spawn 실패 등) — 운영자가 서버 쪽을 봐야 한다.
    Internal(String),
}

impl From<XBackupError> for SubmitError {
    fn from(e: XBackupError) -> Self {
        match &e {
            XBackupError::Usage(_) => SubmitError::Validation(e.detail()),
            _ => SubmitError::Internal(e.to_string()),
        }
    }
}

/// 검증 잡 하나를 spawn하고, 완료를 기다려 stdout·stderr **전체**를 잡 로그에 적재하는
/// 백그라운드 태스크를 띄운다. 반환된 뒤에도 잡은 계속 돈다.
async fn start_verify_job(
    ctx: &Arc<ServeConfig>,
    spec: JobSpec,
) -> std::result::Result<JobId, SubmitError> {
    let masked_args = spec.masked_args(ctx.jobs.secret_registry());
    let audit_action = spec.audit_action();
    let audit_target = spec.audit_target().to_string();

    // 감사 기록이 실패하면 spawn 자체를 막는다 — "기록되지 않은 실행"을 만들지 않기
    // 위해서다(`routes::backup::spawn_tracked_job` 헤더와 같은 판단).
    ctx.audit
        .record(AuditEvent {
            actor: AUDIT_ACTOR,
            action: audit_action,
            target: &audit_target,
            args_masked: &masked_args,
            outcome: AuditOutcome::Requested,
            exit_code: None,
        })
        .await?;

    // verify는 파괴적이 아니므로 `spawn`(not `spawn_destructive`)을 쓴다(모듈 헤더).
    let running = ctx.jobs.spawn(&spec)?;
    let started_at = running.started_at();
    let pid = running.pid();

    let store = jobs::shared(&ctx.state_dir);
    let job_id = match store
        .start(JobStart {
            command: spec.command().verb(),
            profile: None,
            args_masked: &masked_args,
            started_at,
            pid,
        })
        .await
    {
        Ok(id) => id,
        Err(e) => {
            abandon_untracked_verify_child(
                ctx,
                running,
                &e,
                audit_action,
                &audit_target,
                &masked_args,
            )
            .await;
            return Err(e.into());
        }
    };

    let registry = ctx.jobs.secret_registry().clone();
    let audit = Arc::clone(&ctx.audit);

    tokio::spawn(async move {
        // `wait_with_output`는 짧게 끝나는 잡을 위한 편의 함수다(`RunningJob` doc). verify는
        // stdout에 진행률을 찍지 않으므로(모듈 헤더) 이 함수가 진행률을 놓칠 위험이 없다 —
        // 자식이 stdout/stderr를 닫을 때까지 내부에서 두 파이프를 동시에 비운다.
        let completion = match running.wait_with_output().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(
                    job_id = %job_id,
                    error = %e,
                    "verify 자식 출력 수집 실패 — Unknown으로 종료 기록합니다"
                );
                if let Err(e) = store.finish_persistent(&job_id, JobOutcome::Unknown).await {
                    tracing::warn!(job_id = %job_id, error = %e, "verify 종료 기록 실패");
                }
                if let Err(e) = audit
                    .record(AuditEvent {
                        actor: AUDIT_ACTOR,
                        action: audit_action,
                        target: &audit_target,
                        args_masked: &masked_args,
                        outcome: AuditOutcome::Failure,
                        exit_code: None,
                    })
                    .await
                {
                    tracing::warn!(job_id = %job_id, error = %e, "verify 완료 감사 기록 실패");
                }
                return;
            }
        };

        // stdout·stderr 둘 다 잡 로그에 적재한다 — stdout이 이 화면의 존재 이유다(모듈
        // 헤더). 쓰는 쪽에서 먼저 마스킹하고(t11 관례), 읽는 쪽(`view::verify`)이 한 번 더
        // 마스킹한다(방어 심층화 — `crate::web::state::jobs` 헤더 "마스킹은 이 모듈의
        // 책임이 아니다").
        let masked_stdout = registry.mask(&completion.stdout);
        let masked_stderr = registry.mask(&completion.stderr);
        if !masked_stdout.trim().is_empty() {
            if let Err(e) = store
                .append_log(&job_id, LogStream::Stdout, &masked_stdout)
                .await
            {
                tracing::warn!(job_id = %job_id, error = %e, "verify stdout 적재 실패");
            }
        }
        if !masked_stderr.trim().is_empty() {
            if let Err(e) = store
                .append_log(&job_id, LogStream::Stderr, &masked_stderr)
                .await
            {
                tracing::warn!(job_id = %job_id, error = %e, "verify stderr 적재 실패");
            }
        }

        // 종료 기록이 빠지면 이 잡이 영구히 "실행 중"으로 보인다 — `finish_persistent`가
        // 재시도까지 한다(그 함수 doc).
        if let Err(e) = store.finish_persistent(&job_id, completion.outcome).await {
            tracing::warn!(job_id = %job_id, error = %e, "verify 종료 기록 실패");
        }

        if let Err(e) = audit
            .record(AuditEvent {
                actor: AUDIT_ACTOR,
                action: audit_action,
                target: &audit_target,
                args_masked: &masked_args,
                outcome: completion.outcome.audit_outcome(),
                exit_code: completion.outcome.exit_code(),
            })
            .await
        {
            tracing::warn!(job_id = %job_id, error = %e, "verify 완료 감사 기록 실패");
        }
    });

    Ok(job_id)
}

/// 이력에 기록되지 못한 자식을 정리하고 감사 로그에 그 사실을 남긴다.
///
/// `routes::backup::abandon_untracked_child`와 같은 상황·같은 대응이다(그 함수 doc 참고) —
/// spawn과 이력 기록 사이에서 실패하면 그 자식은 어떤 관리 경로에도 속하지 않으므로 여기서
/// 확실히 회수한다.
async fn abandon_untracked_verify_child(
    ctx: &Arc<ServeConfig>,
    running: RunningJob,
    cause: &XBackupError,
    audit_action: &'static str,
    audit_target: &str,
    masked_args: &[String],
) {
    let pid = running.handle().pid;
    match lifecycle::terminate_just_spawned(running, lifecycle::CANCEL_GRACE_DEFAULT).await {
        Ok(outcome) => tracing::error!(
            pid = ?pid,
            cause = %cause,
            cleanup = ?outcome,
            "verify 잡 이력 기록에 실패해 방금 띄운 자식을 정리했습니다 — 이 실행은 일어나지 \
             않은 것으로 취급하세요. state 디렉터리의 디스크 여유·권한을 확인하세요."
        ),
        Err(cleanup_error) => tracing::error!(
            pid = ?pid,
            cause = %cause,
            error = %cleanup_error,
            "verify 잡 이력 기록에 실패한 뒤 자식 정리까지 실패했습니다 — 이 pid가 계속 돌 수 \
             있습니다. `ps`로 확인해 직접 종료하세요."
        ),
    }

    if let Err(e) = ctx
        .audit
        .record(AuditEvent {
            actor: AUDIT_ACTOR,
            action: audit_action,
            target: audit_target,
            args_masked: masked_args,
            outcome: AuditOutcome::Failure,
            exit_code: None,
        })
        .await
    {
        tracing::warn!(error = %e, "이력 기록 실패에 대한 완료 감사 기록도 남기지 못했습니다");
    }
}

// ---------------------------------------------------------------------------
// 제출 처리
// ---------------------------------------------------------------------------

async fn handle_submit(
    ctx: &Arc<ServeConfig>,
    form: &FormBody,
) -> std::result::Result<JobId, SubmitError> {
    let raw_id = form.get(FIELD_ID).unwrap_or_default();
    let mut spec = JobSpec::new(JobCommand::Verify, ctx.lang).with_backup_id(raw_id)?;
    if form.flag(FIELD_DEEP) {
        spec = spec.with_flag(JobFlag::Deep);
    }
    if form.flag(FIELD_CHAIN) {
        spec = spec.with_flag(JobFlag::Chain);
    }
    start_verify_job(ctx, spec).await
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /verify` — 폼. `?id=`가 있으면 백업 ID를 프리필한다(카탈로그 화면에서 오는 링크).
pub async fn form(State(ctx): State<Arc<ServeConfig>>, uri: Uri) -> Markup {
    let prefill_id = query_param(uri.query(), FIELD_ID);
    let target_profile = resolve_target_profile(&ctx);
    layout::shell(
        ctx.lang,
        VERIFY_TITLE,
        view::form_body(ctx.lang, prefill_id.as_deref(), &target_profile, None),
    )
}

/// `POST /verify` — 제출. 성공하면 결과 화면으로 303 리다이렉트한다(POST-Redirect-GET —
/// `routes::backup::submit`과 같은 패턴, 새로고침이 이중 제출을 만들지 않는다).
pub async fn submit(State(ctx): State<Arc<ServeConfig>>, body: String) -> Response {
    let form_body = FormBody::parse(&body);
    match handle_submit(&ctx, &form_body).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, verify_result_href(&id))],
        )
            .into_response(),
        Err(SubmitError::Validation(message)) => {
            let target_profile = resolve_target_profile(&ctx);
            let prefill_id = form_body.get(FIELD_ID).map(str::to_string);
            let notice = view::validation_notice(ctx.lang, &message);
            (
                StatusCode::BAD_REQUEST,
                layout::shell(
                    ctx.lang,
                    VERIFY_TITLE,
                    view::form_body(
                        ctx.lang,
                        prefill_id.as_deref(),
                        &target_profile,
                        Some(notice),
                    ),
                ),
            )
                .into_response()
        }
        Err(SubmitError::Internal(message)) => {
            let target_profile = resolve_target_profile(&ctx);
            let prefill_id = form_body.get(FIELD_ID).map(str::to_string);
            let notice = view::validation_notice(ctx.lang, &message);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                layout::shell(
                    ctx.lang,
                    VERIFY_TITLE,
                    view::form_body(
                        ctx.lang,
                        prefill_id.as_deref(),
                        &target_profile,
                        Some(notice),
                    ),
                ),
            )
                .into_response()
        }
    }
}

/// `GET /verify/{job_id}` — 결과(또는 진행 중) 화면.
pub async fn result(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, VERIFY_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    if detail.summary.is_none() && !detail.log_file_present {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, VERIFY_TITLE, view::unknown_job(ctx.lang)),
        )
            .into_response();
    }
    let registry = request_registry(&ctx);
    let body = view::result_body(ctx.lang, &id, &detail, &registry);
    (StatusCode::OK, layout::shell(ctx.lang, VERIFY_TITLE, body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::web::auth::{self, AuthState};
    use axum::body::Body;
    use axum::http::{header as http_header, Request};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token-verify-4d2a";

    /// 이 모듈의 라우트 테스트가 공유하는 인증 상태와 **그 상태에서 유효한 세션 ID**.
    ///
    /// 세션 도입 뒤로 "인증을 통과한 요청"은 토큰을 쿠키에 넣어서 만들 수 없다 — 그게 정확히
    /// 고친 취약점이다(`crate::web::auth` 모듈 헤더 "쿠키 값은 토큰이 아니다"). 세션은 발급한
    /// 인스턴스에만 있으므로 미들웨어와 쿠키가 **같은** `AuthState`를 봐야 하고, 둘을 하나로
    /// 묶어 두면 그 실수를 할 수 없다.
    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    /// 유효한 세션 쿠키 값 — 인증을 통과해야 하는 요청이 싣는다.
    fn session() -> &'static str {
        &auth_and_session().1
    }

    /// 이 화면 셋만 담은 최소 라우터(`routes::jobs`·`routes::backup`의 테스트 패턴과
    /// 동일 — 리더가 붙일 모양 그대로 여기서 조립한다).
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(VERIFY_PATH, get(form).post(submit))
            .route(VERIFY_RESULT_ROUTE, get(result))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .with_state(ctx)
    }

    async fn get_req(
        ctx: Arc<ServeConfig>,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = cookie {
            builder = builder.header(
                http_header::COOKIE,
                format!("{}={token}", auth::SESSION_COOKIE_NAME),
            );
        }
        let response = router(ctx)
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .expect("라우터 호출 실패");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    async fn post_req(ctx: Arc<ServeConfig>, body: &str, cookie: Option<&str>) -> Response {
        let mut builder = Request::builder().method("POST").uri(VERIFY_PATH).header(
            http_header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        );
        if let Some(token) = cookie {
            builder = builder.header(
                http_header::COOKIE,
                format!("{}={token}", auth::SESSION_COOKIE_NAME),
            );
        }
        router(ctx)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .expect("라우터 호출 실패")
    }

    fn some_backup_id() -> String {
        uuid::Uuid::now_v7().to_string()
    }

    // ---- 인증 ----

    /// 폼은 인증 없이는 401이고, 쿠키가 있으면 200이다.
    #[tokio::test]
    async fn form_requires_auth_then_renders() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = get_req(Arc::clone(&ctx), VERIFY_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.contains(VERIFY_TITLE), "미인증 응답이 화면을 흘렸다");

        let (status, body) = get_req(ctx, VERIFY_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(r#"name="id""#), "id 입력 필드 누락: {body}");
        assert!(body.contains(r#"name="deep""#));
        assert!(body.contains(r#"name="chain""#));
    }

    /// `?id=`는 인증된 폼에서 그대로 프리필된다.
    #[tokio::test]
    async fn query_id_prefills_the_form() {
        let ctx = Arc::new(ServeConfig::for_test());
        let id = some_backup_id();
        let (status, body) = get_req(ctx, &format!("{VERIFY_PATH}?id={id}"), Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&id), "프리필된 id 누락: {body}");
    }

    /// 결과 화면도 인증 없이는 401이다.
    #[tokio::test]
    async fn result_requires_auth() {
        let ctx = Arc::new(ServeConfig::for_test());
        let missing = JobId::generate();
        let (status, _) = get_req(ctx, &verify_result_href(&missing), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    // ---- argv 조립: 세 모드가 각각 다른 argv를 만든다 ----

    #[test]
    fn structural_mode_has_no_deep_or_chain_flags() {
        let id = some_backup_id();
        let spec = JobSpec::new(JobCommand::Verify, Lang::En)
            .with_backup_id(&id)
            .unwrap();
        let argv = spec.to_argv();
        assert_eq!(argv[0], "verify");
        assert!(argv.iter().any(|a| a == "--id"));
        assert!(!argv.iter().any(|a| a == "--deep"));
        assert!(!argv.iter().any(|a| a == "--chain"));
        assert_eq!(argv.last().unwrap(), "--json");
    }

    #[test]
    fn deep_mode_adds_only_the_deep_flag() {
        let id = some_backup_id();
        let spec = JobSpec::new(JobCommand::Verify, Lang::En)
            .with_backup_id(&id)
            .unwrap()
            .with_flag(JobFlag::Deep);
        let argv = spec.to_argv();
        assert!(argv.iter().any(|a| a == "--deep"));
        assert!(!argv.iter().any(|a| a == "--chain"));
    }

    #[test]
    fn chain_mode_adds_only_the_chain_flag() {
        let id = some_backup_id();
        let spec = JobSpec::new(JobCommand::Verify, Lang::En)
            .with_backup_id(&id)
            .unwrap()
            .with_flag(JobFlag::Chain);
        let argv = spec.to_argv();
        assert!(argv.iter().any(|a| a == "--chain"));
        assert!(!argv.iter().any(|a| a == "--deep"));
    }

    #[test]
    fn deep_and_chain_can_combine() {
        let id = some_backup_id();
        let spec = JobSpec::new(JobCommand::Verify, Lang::En)
            .with_backup_id(&id)
            .unwrap()
            .with_flag(JobFlag::Deep)
            .with_flag(JobFlag::Chain);
        let argv = spec.to_argv();
        assert!(argv.iter().any(|a| a == "--deep"));
        assert!(argv.iter().any(|a| a == "--chain"));
    }

    /// verify는 `--profile`을 갖지 않는다(현 CLI 표면) — 감사 target은 항상 `-`다.
    #[test]
    fn verify_spec_has_no_profile_and_dash_audit_target() {
        let id = some_backup_id();
        let spec = JobSpec::new(JobCommand::Verify, Lang::En)
            .with_backup_id(&id)
            .unwrap();
        assert_eq!(spec.audit_target(), "-");
        assert!(!spec.to_argv().iter().any(|a| a == "--profile"));
    }

    /// 잘못된 백업 ID는 spawn 이전에 거부된다(자식이 뜨지 않는다).
    #[tokio::test]
    async fn invalid_backup_id_is_rejected_before_spawn() {
        let ctx = Arc::new(ServeConfig::for_test());
        let response = post_req(ctx, "id=--force&deep=", Some(session())).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains(VERIFY_TITLE));
    }

    /// 빈 id는 거부된다.
    #[tokio::test]
    async fn empty_backup_id_is_rejected() {
        let ctx = Arc::new(ServeConfig::for_test());
        let response = post_req(ctx, "id=", Some(session())).await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // ---- 파싱: 손상·빈·비-UTF8·스키마 불일치 입력에서 패닉 없음 ----

    const SAMPLE_JSON: &str = r#"{"schema":1,"backup_id":"bk","manifest_sidecar_ok":true,"data_checksum_ok":true,"deep_decode_ok":null,"empty_slice":false,"warnings":[],"ok":true,"chain":null}"#;

    #[test]
    fn sample_doc_parses() {
        let doc = parse_verify_doc(SAMPLE_JSON).expect("표본이 파싱되지 않는다");
        assert_eq!(doc.backup_id, "bk");
        assert!(doc.manifest_sidecar_ok);
        assert!(doc.chain.is_none());
    }

    /// 병리적으로 깊은 stdout은 `serde_json`에 **닿기 전에** 거부된다
    /// ([`crate::web::jsonguard`]). 이 화면은 검증 결과만 못 그릴 뿐 검증 자체는 이미
    /// 끝났으므로, 종료 코드로 판단할 수 있다는 것이 안내 문장의 요지다.
    #[test]
    fn pathologically_deep_stdout_is_refused_before_parsing() {
        let bomb = "[".repeat(50_000);
        assert_eq!(
            parse_verify_doc(&bomb),
            Err(ParseProblem::TooDeep {
                found: 50_000,
                max: jsonguard::MAX_JSON_DEPTH,
            })
        );
        // 상한 안쪽은 깊이를 이유로 거부되지 않는다.
        let nested = format!(
            r#"{{"schema":{EXPECTED_VERIFY_SCHEMA},"backup_id":"x","deep":{}{}}}"#,
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(
            !matches!(parse_verify_doc(&nested), Err(ParseProblem::TooDeep { .. })),
            "상한 안쪽 문서가 깊이를 이유로 거부됐다"
        );
    }

    #[test]
    fn broken_stdout_becomes_readable_errors_without_panicking() {
        assert_eq!(parse_verify_doc(""), Err(ParseProblem::Empty));
        assert_eq!(parse_verify_doc("   \n\t "), Err(ParseProblem::Empty));
        assert!(matches!(
            parse_verify_doc("not json at all"),
            Err(ParseProblem::Malformed(_))
        ));
        assert!(matches!(
            parse_verify_doc("{\"schema\":1,"),
            Err(ParseProblem::Malformed(_))
        ));
        assert!(matches!(
            parse_verify_doc("{}"),
            Err(ParseProblem::Malformed(_))
        ));
        assert_eq!(
            parse_verify_doc(r#"{"schema":99,"backup_id":"x"}"#),
            Err(ParseProblem::SchemaMismatch {
                found: 99,
                expected: EXPECTED_VERIFY_SCHEMA
            })
        );
        // 비-UTF8 바이트는 `RunningJob::wait_with_output`가 이미 lossy 변환을 마쳤다는
        // 전제이므로, 여기서는 그 변환 결과(대체 문자 포함 문자열)를 흉내낸다.
        let lossy = String::from_utf8_lossy(&[b'{', 0xFF, b'}']).into_owned();
        assert!(matches!(
            parse_verify_doc(&lossy),
            Err(ParseProblem::Malformed(_))
        ));
    }

    // ---- deep 키 미설정 감지 ----

    #[test]
    fn deep_key_missing_detects_age_and_aes_env_names() {
        let age_msg = format!(
            "age 암호화 백업의 복호화에는 개인키가 필요합니다 — {}에 identity 파일 경로를 \
             지정하세요(§8.5 키 격리) — verify --deep은 개인키 보유 호스트에서 실행하세요\
             (§8.5 키 격리)",
            crate::pipeline::stage::ENV_AGE_IDENTITY_FILE
        );
        assert!(deep_key_missing(true, "failed", &age_msg));

        let aes_msg = format!(
            "aes-256-gcm 백업의 복호화에는 대칭 키가 필요합니다 — {}에 32바이트 hex 키를 \
             지정하세요",
            crate::pipeline::stage::ENV_AES_KEY_HEX
        );
        assert!(deep_key_missing(true, "failed", &aes_msg));

        // deep을 안 걸었으면 같은 텍스트여도 이 경로가 아니다.
        assert!(!deep_key_missing(false, "failed", &age_msg));
        // 결과가 failed가 아니면(예: succeeded-with-warnings) 이 경로가 아니다.
        assert!(!deep_key_missing(true, "succeeded-with-warnings", &age_msg));
        // 관련 텍스트가 없으면 감지되지 않는다.
        assert!(!deep_key_missing(true, "failed", "checksum mismatch"));
    }

    /// 감지에 쓰는 텍스트에는 파일 **경로**가 없다 — env 부재 메시지는 이름만 담는다는
    /// 전제를 여기서도 고정해 둔다(값이 있다면 이 테스트가 그 값을 지목했을 것이다).
    #[test]
    fn deep_key_missing_message_never_needs_a_path_value() {
        let msg = format!("... {} ...", crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);
        assert!(deep_key_missing(true, "failed", &msg));
        assert!(
            !msg.contains('/'),
            "테스트 메시지 자체에 경로가 섞이면 안 된다: {msg}"
        );
    }

    // ---- argv 값 추출 헬퍼 ----

    #[test]
    fn arg_value_and_flag_helpers_read_masked_argv() {
        let args = vec![
            "verify".to_string(),
            "--id".to_string(),
            "bk-1".to_string(),
            "--deep".to_string(),
            "--json".to_string(),
        ];
        assert_eq!(arg_value(&args, "--id"), Some("bk-1"));
        assert_eq!(arg_value(&args, "--missing"), None);
        assert!(args_have_flag(&args, "--deep"));
        assert!(!args_have_flag(&args, "--chain"));
    }

    // ---- 대상 프로파일 표시 ----

    #[test]
    fn resolve_target_profile_reads_config_default() {
        let mut cfg = ServeConfig::for_test();
        assert_eq!(resolve_target_profile(&cfg), DEFAULT_PROFILE_DISPLAY);

        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(&config_path, "default_profile = \"prod\"\n").unwrap();
        cfg.config_path = Some(config_path);
        assert_eq!(resolve_target_profile(&cfg), "prod");
    }

    // ---- 왕복: spawn(오프라인 doctor로) → 이력 기록 → stdout 적재까지 확인 ----

    /// 실제 DB 없이 도는 `doctor --json`을 verify 명령인 척 흘려보내는 대신, 여기서는
    /// verify 자체가 오프라인(구조 검증, 존재하지 않는 백업 ID → 빠른 exit 1)로 끝나는
    /// 성질을 이용한다: config 없이 뜬 러너로 verify를 spawn하면 자식은 config를 못 찾아
    /// exit 2(Rejected)로 빠르게 끝난다. 이 테스트는 spawn → 백그라운드 완료 처리 →
    /// 결과 화면 렌더까지 실제로 관통하는지만 본다(stdout 파싱 성공 여부는 다른 테스트가
    /// 이미 순수 함수로 고정했다).
    #[tokio::test]
    async fn submit_then_result_round_trips_through_real_child() {
        let ctx = Arc::new(ServeConfig::for_test());
        let id = some_backup_id();
        let response = post_req(ctx.clone(), &format!("id={id}"), Some(session())).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let location = response
            .headers()
            .get(http_header::LOCATION)
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(location.starts_with(VERIFY_PATH));

        // 자식이 끝나고 이력이 기록될 때까지 짧게 폴링한다(실제 프로세스이므로 즉시는
        // 아닐 수 있다).
        let job_id = location.rsplit('/').next().unwrap().to_string();
        let mut settled = false;
        for _ in 0..100 {
            let (status, body) = get_req(ctx.clone(), &location, Some(session())).await;
            assert_eq!(status, StatusCode::OK, "job {job_id}: {body}");
            if !body.contains("Verification in progress") && !body.contains("검증 진행 중") {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(settled, "잡이 시간 안에 끝나지 않았다(job {job_id})");
    }

    /// 형식이 틀린 잡 id는 400, 모르는 잡 id는 404다.
    #[tokio::test]
    async fn malformed_and_unknown_job_ids() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, _) = get_req(
            ctx.clone(),
            &format!("{VERIFY_PATH}/not-a-uuid"),
            Some(session()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);

        let missing = JobId::generate();
        let (status, body) = get_req(ctx, &verify_result_href(&missing), Some(session())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.contains("Unknown job") || body.contains("알 수 없는 잡"));
    }
}
