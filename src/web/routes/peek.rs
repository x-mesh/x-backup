//! `GET /peek` — 데이터 육안 확인 화면. `POST /peek/reveal` — 원문 노출(감사 기록 필수).
//!
//! ## 이 화면이 다른 6개 화면과 성질이 다른 이유
//! 다른 화면(`doctor`·`backup`·`config`·`jobs`·`lock`)이 다루는 것은 백업 id·크기·상태·
//! 설정값 같은 **메타데이터**뿐이다. 이 화면은 `x-backup peek`을 그대로 웹으로 옮긴
//! 것이라 **실제 DB 문서/행 원문**을 브라우저로 옮긴다. 그래서 PII가 브라우저 캐시·
//! 스크린샷·개발자도구 히스토리·프록시 로그로 번지는 표면이 이 화면에서 처음 생긴다.
//! 이 파일과 [`crate::web::view::peek`]의 모든 설계 결정은 그 사실 하나에서 나온다.
//!
//! ## 기본 마스킹 — 근거는 [`crate::web::view::peek`] 헤더
//! 값은 항상 [`view::shape_markup`](크레이트 경로: `crate::web::view::peek::shape_markup`)로
//! 그린다(타입·길이만). 원문은 [`view::revealed_markup`]로만 그리고, 그 함수는 **오직**
//! [`reveal`](이 파일의 `POST /peek/reveal` 핸들러)가 감사 기록에 성공한 뒤에만 부른다.
//! `GET /peek`는 어떤 쿼리 조합을 줘도 원문을 보여줄 방법이 없다 — 원문 렌더 경로 자체가
//! `reveal` 함수 안에만 있다.
//!
//! ## 원문 노출을 감사 로그에 기록하는 이유, 그리고 `record()`를 쓴 이유
//! 원문 노출은 되돌릴 수 없다 — 화면에 한 번 그려지면 스크린샷이든 눈으로 본 것이든
//! 이미 노출된 것이고, 로그가 사후에 그것을 지울 수 없다. 그래서 "누가·언제·어느
//! 네임스페이스를 열람했는가"는 반드시 기록되어야 한다([`AuditLog`] 헤더의 존재 이유 —
//! 침해를 막지 못해도 사후 추적은 가능해야 한다).
//!
//! **`gate()`가 아니라 `record()`를 쓴다.** [`crate::web::audit::AuditLog::gate`]는 **딱 한
//! 가지 계약**만 강제한다 — "append에 성공했을 때만 [`AuditReceipt`]를 내주고, 그 receipt를
//! 값으로 소비해야만 [`JobRunner::spawn_destructive`]를 호출할 수 있다"(타입 강제, t8 헤더).
//! 이 화면의 원문 노출은 그 어떤 `spawn_destructive` 호출도 만들지 않는다 — `peek`
//! 명령은 [`JobCommand::Peek::is_destructive`]가 `false`이므로 애초에 `JobRunner::spawn`
//! (비파괴 경로)만으로 실행되고, "원문을 보여줄지"는 **이미 받아온 데이터를 어떻게
//! 렌더할지**를 고르는 문제이지 "새로 무엇을 실행할지"를 고르는 문제가 아니다. `gate()`를
//! 억지로 끼워 넣으려면 receipt를 소비할 `spawn_destructive` 호출이 필요한데, 여기엔 그
//! 자리가 없다 — `gate()`는 이 화면이 푸는 문제(렌더 여부 게이팅)에 맞는 도구가 아니다.
//!
//! [`routes::backup`](`crate::web::routes::backup`) 헤더가 이미 같은 결론에 도달했다:
//! "backup은 파괴적이지 않지만 감사 대상이다 → 비파괴 기록 전용 API는
//! [`AuditLog::record`]다." 이 화면도 같은 처지다(비파괴 명령, 그러나 감사가 필요한
//! 부작용이 있다). 다른 점은 backup은 "잡을 시작했다"는 사실을 기록하고, 이 화면은
//! "원문을 보여줬다"는 사실을 기록한다는 것뿐이다. 강제하는 규율은 손으로 지킨다 —
//! [`reveal`] 함수 안에서 `ctx.audit.record(...).await?`가 **원문 마크업을 만들기 전에**
//! 먼저 실행되고, 실패하면 그 자리에서 에러 응답으로 끊긴다(원문은 만들어지지도 않는다).
//! 컴파일러가 강제하는 `gate()`만큼 강하지는 않지만(호출 순서를 사람이 지켜야 한다),
//! 이 화면이 푸는 문제 자체가 "실행을 막는다"가 아니라 "렌더를 막는다"라 애초에 타입으로
//! 막을 대상(제2의 `spawn` 호출)이 없다.
//!
//! ## 개요(overview)에서는 원문 노출을 허용하지 않는다
//! `--ns` 없는 개요는 **여러 네임스페이스**의 최신 문서를 한 번에 보여준다. 거기서
//! 원문 노출을 허용하면 클릭 한 번으로 서로 다른 컬렉션 여러 개의 데이터가 동시에
//! 노출되고, 감사 로그 한 줄이 "어느 네임스페이스"를 특정하지 못하게 된다(리더 지시서가
//! 요구한 열람 기록 항목 그대로). 그래서 [`reveal`]은 `ns` 파라미터를 **필수**로 요구하고,
//! 비어 있으면 400으로 거부한다 — 노출 단위를 항상 네임스페이스 하나로 좁힌다.
//!
//! ## 토글을 GET 쿼리가 아니라 POST로 둔 이유
//! 프로파일/네임스페이스 **탐색**(`GET /peek?profile=…&ns=…`)은 안전한 내비게이션이라
//! 북마크·새로고침·히스토리가 자유로워야 한다 — 그건 메타데이터만 마스킹해서 보여주므로
//! 문제가 없다. 하지만 "원문을 보여달라"는 그 자체로 되돌릴 수 없는 노출 행위이고, GET은
//! HTTP 규약상 **안전(safe)·부작용 없음**이 전제다. 그 전제가 깨지는 순간 세 가지가
//! 조용히 위험해진다:
//!
//! 1. 브라우저·확장·중간 프록시의 **프리페치**가 사람의 클릭 없이 GET을 미리 쏠 수 있다 —
//!    "보기" 버튼에 마우스를 올리기만 해도 원문이 노출될 수 있다는 뜻이다.
//! 2. `?reveal=1` 같은 쿼리가 브라우저 히스토리·북마크·리퍼러 헤더·프록시 접근 로그에
//!    남으면, 그 URL을 다시 여는 것만으로(또는 Slack에 붙여넣기만 해도) 원문이 재현된다.
//!    쿼리 문자열 자체는 값을 담지 않더라도(우리는 어차피 값을 URL에 싣지 않는다) "이
//!    URL을 열면 원문이 나온다"는 사실 자체가 재현 가능한 노출 경로가 된다.
//! 3. [`crate::web::auth::require_same_origin`]의 CSRF 방어는
//!    [`crate::web::auth::is_state_changing`]이 GET/HEAD/OPTIONS를 **면제한다** — GET으로
//!    만들면 그 방어선 밖에 남는다. 아직 라우터에 배선되지 않았더라도(그 헤더 참조),
//!    POST로 만들어 두면 배선되는 순간 이 화면도 자동으로 보호권 안에 들어온다.
//!
//! 그래서 원문 노출은 `POST /peek/reveal`뿐이고, 폼(`view::peek::namespace_body`)이
//! `profile`/`ns`/`limit`을 hidden 필드로 실어 보낸다. 응답은 리다이렉트하지 않고(303으로
//! `GET`에 태우면 위 2번 문제가 다시 생긴다) **그 POST 응답 자체**에 원문을 그려 돌려주고,
//! `Cache-Control: no-store`를 강제한다 — 새로고침(=GET)하면 다시 마스킹된 화면으로
//! 돌아간다.
//!
//! ## `Cache-Control: no-store`의 적용 범위 — 마스킹 응답에도 붙인다
//! 리더 지시서는 원문 응답에 대해서만 물었지만, 이 화면은 **마스킹 응답에도** 예외 없이
//! 붙인다([`no_store`]가 모든 핸들러 반환 경로를 감싼다). 문서 수·네임스페이스 이름·필드
//! 이름조차 "이 운영 DB에 무엇이 들어 있는가"라는 정보이고, 이 화면은 인증된 운영
//! 콘솔이지 공개 캐시 대상이 아니다. 두 응답에 서로 다른 캐시 정책을 두면 나중에 실수로
//! 뒤바뀔 여지를 만들 뿐 얻는 것이 없다 — 하나로 통일해 실수의 여지를 없앤다.
//!
//! ## 적대적/병리적 입력 방어
//! 이 화면이 그리는 문서는 운영 DB에서 온 신뢰할 수 없는 데이터다. 깊이·필드 수·배열
//! 길이·문자열 길이 상한과 그 근거표는 [`crate::web::view::peek`] 헤더에 있다. 이 파일이
//! 추가로 지키는 것 하나: **자식 stdout을 `serde_json::Value`로 파싱하기 전에** 원문
//! 텍스트의 중첩 깊이를 [`crate::web::jsonguard::check_depth`]로 먼저 재고, 상한을 넘으면
//! 파싱을 건너뛰고 [`ReportError::TooDeep`]으로 접는다.
//!
//! **정정(원래 근거는 틀렸다):** 이 검사는 t20이 "깊은 JSON은 스택 오버플로로 프로세스를
//! 하드 abort시킨다"는 전제로 넣었는데, 그 전제는 사실이 아니다 — `serde_json`이 기본으로
//! 재귀 상한을 켜고 들어와 깊은 입력을 평범한 `Err`로 접는다(실측은
//! [`crate::web::jsonguard`] 헤더). 그래서 이 검사는 **크래시 방어가 아니라 진단 개선**이다:
//! 실제 깊이를 숫자로 보여줘 운영자가 "스키마가 한 겹 깊어졌나"와 "데이터가 병리적인가"를
//! 가를 수 있게 한다.
//!
//! ## 웹은 자식 프로세스로만 데이터를 얻는다
//! [`crate::engine`]·[`crate::storage`]·`crypto`를 이 파일에서 직접 부르지 않는다
//! ([`crate::web`] 최상위 불변식). `peek --json` 자식을 [`JobRunner::spawn`]으로 띄우고
//! 그 stdout만 읽는다 — `doctor`([`crate::web::routes::doctor`])와 같은 실행 패턴이지만,
//! `doctor`는 오프라인 점검이라 20초로 충분한 반면 `peek`는 실제로 DB에 왕복하므로
//! [`PEEK_TIMEOUT`]을 더 길게 잡는다(그 상수 doc 참조).
//!
//! ## 시크릿
//! 자식 stdout에 config env 시크릿 원문이 섞일 여지는 설계상 없지만(`peek --json`은
//! DB 문서만 싣는다), 자식이 실패했을 때의 stderr 진단 메시지에는 무엇이 나올지 이
//! 프로세스가 통제할 수 없다. 그래서 stderr는 항상 `ctx.jobs.secret_registry()` 기반
//! 레지스트리를 통과한 뒤에만 화면에 얹는다(`routes::doctor`와 같은 판단). 원문 문서 값
//! 자체도 같은 레지스트리를 한 번 더 통과한다 — 남의 DB 필드에 우연히 등록된 시크릿과
//! 정확히 일치하는 값이 들어있는 병적인 경우까지 잡기 위해서다
//! (`crate::web::view::peek` 헤더 "객체 키는 지우지 않는다" 절 참조).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Extension, State};
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use maud::{html, Markup};
use serde_json::Value;

use crate::error::Result;
use crate::i18n::Lang;
use crate::web::audit::{AuditEvent, AuditOutcome};
use crate::web::auth::AuthState;
use crate::web::job::args::ProfileName;
use crate::web::job::{JobCommand, JobCount, JobOutcome, JobRunner, JobSpec, RunningJob};
use crate::web::jsonguard;
use crate::web::mask::SecretRegistry;
use crate::web::view::peek::{self as view, NamespaceSummary};
use crate::web::view::{components, layout};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수 — 라우터(리더)·마크업·테스트가 공유한다
// ---------------------------------------------------------------------------

/// 조회 화면 경로(GET) — 프로파일/네임스페이스 선택과 마스킹된 데이터를 함께 다룬다.
pub const PEEK_PATH: &str = "/peek";
/// 원문 노출 제출 경로(POST 전용). 모듈 헤더 "토글을 GET 쿼리가 아니라 POST로 둔 이유" 참조.
pub const PEEK_REVEAL_PATH: &str = "/peek/reveal";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const PEEK_TITLE: &str = "Peek";

/// 감사 로그 `actor` 값. 이 콘솔에는 세션 토큰 하나뿐이라 사용자별 신원이 없다 —
/// `routes::backup`·`routes::config`와 같은 값을 쓴다.
const AUDIT_ACTOR: &str = "web";
/// 원문 노출 이벤트의 감사 `action` 값. `<verb>.<subaction>` 관행을 따른다
/// ([`JobCommand::audit_action`]의 `<verb>.run`과 구분되는 별도 계열 — 노출은 잡 실행이
/// 아니라 렌더 결정이므로 접미사를 다르게 둔다).
const AUDIT_ACTION_REVEAL: &str = "peek.reveal";

/// `peek` 자식 실행 상한.
///
/// `doctor`(20초, 오프라인 config 점검뿐)와 달리 `peek`는 실제로 DB에 접속해 최신 문서를
/// 읽어온다 — 네트워크 왕복·인증·쿼리 실행이 낀다. 30초는 정상적인 원격 DB 왕복의
/// 여유(로컬은 수십~수백 ms, 원격 리전 간 접속도 수 초를 넘기지 않는 것이 보통)를 넉넉히
/// 덮으면서도, 프런트 리버스 프록시의 흔한 기본 `proxy_read_timeout`(nginx 60초)보다는
/// 짧게 잡아 우리가 만든 읽을 수 있는 타임아웃 화면이 나가게 한다(`routes::doctor`
/// [`DOCTOR_TIMEOUT`](crate::web::routes::doctor)와 같은 근거 구조).
const PEEK_TIMEOUT: Duration = Duration::from_secs(30);

/// `--limit` 미지정 시 기본값 — CLI 기본값과 맞춘다 필요는 없다(웹은 화면에 맞는 기본을
/// 고를 수 있다, `JobSpec` 헤더 "의미 판정은 CLI가 한다"와는 다른 층위 — 이건 UI 기본값
/// 선택이지 도메인 판정이 아니다). 10건이면 스크롤 없이 한 화면에 들어오는 양이다.
const DEFAULT_LIMIT: u32 = 10;
/// `--limit` 상한. 문서 하나가 [`view::MAX_FIELDS_SHOWN`]개 필드를 가질 수 있는 상황에서
/// 문서 수까지 무제한으로 열면 응답 하나가 지나치게 커진다 — 25건은 개요를 파악하기에
/// 충분하면서 한 화면 분량을 유지하는 선이다.
const MAX_LIMIT: u32 = 25;

const FIELD_PROFILE: &str = "profile";
const FIELD_NS: &str = "ns";
const FIELD_LIMIT: &str = "limit";

/// `GET /peek` 링크(네임스페이스 개요 — 원문 없음, 항상 안전한 내비게이션).
pub fn overview_href(profile: &str) -> String {
    format!("{PEEK_PATH}?{FIELD_PROFILE}={}", url_encode(profile))
}

/// `GET /peek` 링크(네임스페이스 상세 — 마스킹만, 원문 없음).
pub fn namespace_href(profile: &str, ns: &str) -> String {
    format!(
        "{PEEK_PATH}?{FIELD_PROFILE}={}&{FIELD_NS}={}",
        url_encode(profile),
        url_encode(ns)
    )
}

/// 값을 URL 쿼리에 안전하게 싣기 위한 최소 percent-encode. 프로파일/네임스페이스 값은
/// 이미 [`crate::web::job::args`]의 화이트리스트(영숫자/`_`/`-`/`.`)를 통과한 것들이라
/// 실질적으로 인코딩될 문자가 없지만, 그 전제에만 기대지 않는다 — 링크를 만드는 이
/// 함수 자체가 항상 안전한 URL을 내도록 방어적으로 인코딩한다.
fn url_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 쿼리/폼 파싱 — `routes::backup::FormBody`와 같은 이유로 로컬 파서를 둔다(그 타입은
// 비공개라 재사용 불가. `axum::Form`을 안 쓰는 이유는 `routes::config` 헤더 참조 —
// 이 화면은 GET 쿼리도 파싱해야 해서 그 이유가 한 겹 더 강하게 적용된다).
// ---------------------------------------------------------------------------

struct Params {
    pairs: Vec<(String, String)>,
}

impl Params {
    fn from_query(uri: &Uri) -> Self {
        Self::parse(uri.query().unwrap_or(""))
    }

    fn from_body(body: &str) -> Self {
        Self::parse(body)
    }

    fn parse(raw: &str) -> Self {
        let mut pairs = Vec::new();
        for pair in raw.as_bytes().split(|&b| b == b'&') {
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
            .filter(|v| !v.is_empty())
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

// ---------------------------------------------------------------------------
// 선택 값 — 쿼리/폼에서 뽑아낸, 아직 검증 전인 요청
// ---------------------------------------------------------------------------

struct Selection {
    profile: String,
    ns: Option<String>,
    limit: u32,
}

fn parse_limit(params: &Params) -> u32 {
    params
        .get(FIELD_LIMIT)
        .and_then(|v| v.parse::<u32>().ok())
        .map(|v| v.clamp(1, MAX_LIMIT))
        .unwrap_or(DEFAULT_LIMIT)
}

fn selection_from(params: &Params) -> Option<Selection> {
    let profile = params.get(FIELD_PROFILE)?.to_string();
    Some(Selection {
        profile,
        ns: params.get(FIELD_NS).map(str::to_string),
        limit: parse_limit(params),
    })
}

/// 선택 값으로 `JobSpec`을 조립한다. 값 검증(프로파일명·네임스페이스 형태)은
/// [`JobSpec`]의 빌더가 [`crate::web::job::args`]를 거쳐 수행한다 — 이 함수는 그
/// 실패를 그대로 전파할 뿐 자체 판정을 하지 않는다.
fn peek_spec(selection: &Selection, lang: Lang) -> Result<JobSpec> {
    let profile = ProfileName::parse(&selection.profile, lang)?;
    let mut spec = JobSpec::new(JobCommand::Peek, lang).with_profile(profile);
    if let Some(ns) = &selection.ns {
        spec = spec.with_ns(ns)?;
        spec = spec.with_count(JobCount::Limit, selection.limit);
    }
    Ok(spec)
}

// ---------------------------------------------------------------------------
// 자식 실행 — `routes::doctor`와 같은 패턴(잡 러너 + 요청 수명 상한). 그 파일의 타입은
// 비공개라 재사용할 수 없어 이 화면에 맞게 다시 둔다(모듈 헤더 "웹은 자식 프로세스로만"
// 절 — 실행 패턴 자체는 doctor와 동일해야 웹과 CLI가 다르게 동작하지 않는다).
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum RunError {
    Spawn(String),
    Wait(String),
    Timeout(Duration),
}

impl RunError {
    fn explain(&self, lang: Lang) -> String {
        match self {
            RunError::Spawn(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "Spawning the peek child failed:",
                    "peek 자식 프로세스를 띄우지 못했습니다:",
                )
            ),
            RunError::Wait(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "Reading the peek child's output failed:",
                    "peek 자식 프로세스의 출력을 읽는 중 실패했습니다:",
                )
            ),
            RunError::Timeout(limit) => format!(
                "{} ({}s)",
                lang.sel(
                    "peek did not finish within the time limit and was killed. A stalled or unreachable source is the usual cause.",
                    "peek가 상한 시간 안에 끝나지 않아 종료시켰습니다. 소스에 연결할 수 없거나 멈춘 경우가 흔한 원인입니다.",
                ),
                limit.as_secs()
            ),
        }
    }
}

struct ChildRun {
    outcome: JobOutcome,
    stdout: String,
    stderr: String,
}

async fn run_peek(runner: &JobRunner, spec: &JobSpec) -> std::result::Result<ChildRun, RunError> {
    let running = runner
        .spawn(spec)
        .map_err(|e| RunError::Spawn(e.to_string()))?;
    await_with_limit(running, PEEK_TIMEOUT).await
}

async fn await_with_limit(
    running: RunningJob,
    limit: Duration,
) -> std::result::Result<ChildRun, RunError> {
    let pid = running.pid();
    match tokio::time::timeout(limit, running.wait_with_output()).await {
        Ok(Ok(done)) => Ok(ChildRun {
            outcome: done.outcome,
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

#[cfg(unix)]
fn kill_child(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: pid는 방금 이 프로세스가 spawn한, 아직 수거되지 않은 자식의 pid다.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_child(_pid: Option<u32>) {}

// ---------------------------------------------------------------------------
// 파싱 — 자식 stdout → 보고서 (순수, 부작용 없음)
// ---------------------------------------------------------------------------

/// `peek --json` 스키마 버전. `src/cli/handlers/peek.rs::PEEK_JSON_SCHEMA`와 같은 값이어야
/// 한다 — 그 상수는 `pub`이 아니라(이 태스크 소관 밖 파일) 여기서 값을 복제한다. 어긋나면
/// 아래 `real_peek_output_parses`가 실제 CLI 출력 형태를 표본으로 잡는다.
const EXPECTED_PEEK_SCHEMA: u32 = 1;

#[derive(Debug, Clone)]
enum PeekReport {
    /// `--ns` 없는 개요 — 네임스페이스별 문서 수 + 최신 1건.
    Overview { items: Vec<NamespaceSummary> },
    /// `--ns` 있는 상세 — 그 네임스페이스의 최신 N건. Mongo는 `documents`, PG/MySQL은
    /// `rows` 키를 쓰지만(`src/cli/handlers/peek.rs`의 `documents_json`/`rows_json`) 렌더
    /// 관점에서는 "문서 배열"이라는 점이 같으므로 여기서 하나로 합친다.
    Namespace { ns: String, items: Vec<Value> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ReportError {
    Empty,
    Malformed(String),
    SchemaMismatch {
        found: u64,
        expected: u32,
    },
    /// 파싱을 시도하기도 전에 원문 중첩 깊이가 상한을 넘었다(모듈 헤더 "적대적/병리적
    /// 입력 방어" 참조). 판정은 [`crate::web::jsonguard`]가 한다.
    TooDeep {
        found: usize,
        max: usize,
    },
}

impl ReportError {
    fn explain(&self, lang: Lang) -> String {
        match self {
            ReportError::Empty => lang
                .sel(
                    "peek produced no output. The child may have died before writing anything.",
                    "peek가 아무 출력도 내지 않았습니다. 자식 프로세스가 쓰기 전에 죽었을 수 있습니다.",
                )
                .to_string(),
            ReportError::Malformed(detail) => format!(
                "{} {}",
                lang.sel(
                    "peek output could not be parsed as the expected JSON:",
                    "peek 출력을 기대한 JSON으로 해석할 수 없습니다:",
                ),
                excerpt(detail)
            ),
            ReportError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} ≠ {expected})",
                lang.sel(
                    "peek reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "peek가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            ReportError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "peek output is nested more deeply than this console parses. Nothing was read — suspect the document itself, or whatever generated it.",
                    "peek 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다. 내용을 읽지 않았습니다 — 문서 자체나 그것을 만든 쪽을 의심하세요.",
                )
            ),
        }
    }
}

fn excerpt(text: &str) -> String {
    const EXCERPT_CHARS: usize = 400;
    let trimmed = text.trim();
    if trimmed.chars().count() <= EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(EXCERPT_CHARS).collect();
    format!("{head}…")
}

fn parse_report(text: &str) -> std::result::Result<PeekReport, ReportError> {
    if text.trim().is_empty() {
        return Err(ReportError::Empty);
    }
    jsonguard::check_depth(text).map_err(|d| ReportError::TooDeep {
        found: d.found,
        max: d.max,
    })?;
    let value: Value =
        serde_json::from_str(text).map_err(|e| ReportError::Malformed(e.to_string()))?;
    match value.get("schema").and_then(Value::as_u64) {
        Some(found) if found == u64::from(EXPECTED_PEEK_SCHEMA) => {}
        Some(found) => {
            return Err(ReportError::SchemaMismatch {
                found,
                expected: EXPECTED_PEEK_SCHEMA,
            })
        }
        None => {
            return Err(ReportError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }

    if let Some(namespaces) = value.get("namespaces") {
        let raw_items = namespaces
            .as_array()
            .ok_or_else(|| ReportError::Malformed("`namespaces`가 배열이 아닙니다".to_string()))?;
        let items = raw_items
            .iter()
            .map(overview_item_from_value)
            .collect::<std::result::Result<Vec<_>, _>>()?;
        return Ok(PeekReport::Overview { items });
    }

    let ns = value
        .get("ns")
        .and_then(Value::as_str)
        .ok_or_else(|| ReportError::Malformed("`ns` 필드가 없습니다".to_string()))?
        .to_string();
    let items = value
        .get("documents")
        .or_else(|| value.get("rows"))
        .and_then(Value::as_array)
        .ok_or_else(|| ReportError::Malformed("`documents`/`rows` 필드가 없습니다".to_string()))?
        .clone();
    Ok(PeekReport::Namespace { ns, items })
}

fn overview_item_from_value(value: &Value) -> std::result::Result<NamespaceSummary, ReportError> {
    let ns = value
        .get("ns")
        .and_then(Value::as_str)
        .ok_or_else(|| ReportError::Malformed("네임스페이스 항목에 `ns`가 없습니다".to_string()))?
        .to_string();
    let count = value.get("count").cloned().unwrap_or(Value::Null);
    let latest = value.get("latest").cloned().filter(|v| !v.is_null());
    Ok(NamespaceSummary { ns, count, latest })
}

// ---------------------------------------------------------------------------
// 결과 조립
// ---------------------------------------------------------------------------

enum Outcome {
    Ran {
        outcome: JobOutcome,
        parsed: std::result::Result<PeekReport, ReportError>,
        stderr: String,
    },
    Unavailable {
        error: RunError,
    },
}

async fn collect(runner: &JobRunner, spec: &JobSpec) -> Outcome {
    match run_peek(runner, spec).await {
        Ok(run) => Outcome::Ran {
            outcome: run.outcome,
            parsed: parse_report(&run.stdout),
            stderr: run.stderr,
        },
        Err(error) => Outcome::Unavailable { error },
    }
}

fn request_registry(ctx: &ServeConfig, auth: &AuthState) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    auth.register_session_secret(&mut registry);
    registry
}

fn load_profile_names(ctx: &ServeConfig) -> Vec<String> {
    let Some(path) = &ctx.config_path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::file::Config::from_toml_str(&text) else {
        return Vec::new();
    };
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    names.sort_unstable();
    names
}

/// 응답에 `Cache-Control: no-store`를 강제로 얹는다(모듈 헤더 "적용 범위" 절 — 마스킹
/// 응답에도 예외 없이 붙인다). 이미 있는 헤더는 덮어쓴다.
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
}

fn error_page(lang: Lang, status: StatusCode, notice: Markup) -> Response {
    let body = html! {
        (components::page_head(PEEK_TITLE, None))
        (notice)
    };
    no_store((status, layout::shell(lang, PEEK_TITLE, body)).into_response())
}

/// 아웃컴을 화면으로 접는다. `revealed`가 `true`면 [`view::namespace_body`]가 원문을
/// 그린다 — 이 값은 오직 [`reveal`]에서 감사 기록이 성공한 뒤에만 `true`로 넘어온다.
/// `limit`은 이 요청이 실제로 쓴 값을 그대로 넘긴다 — 원문 노출 폼의 hidden 필드가
/// 다시 같은 값으로 제출되어야, 재요청이 화면에 보인 것과 다른 개수를 가져오지 않는다.
#[allow(clippy::too_many_arguments)]
fn render(
    lang: Lang,
    profile: &str,
    limit: u32,
    outcome: &Outcome,
    revealed: bool,
    registry: &SecretRegistry,
) -> Markup {
    match outcome {
        Outcome::Unavailable { error } => html! {
            (components::page_head(PEEK_TITLE, None))
            (components::notice(components::Level::Error, lang.sel("Could not run peek", "peek를 실행할 수 없습니다"), html! {
                p { (error.explain(lang)) }
            }))
        },
        Outcome::Ran {
            outcome: job_outcome,
            parsed,
            stderr,
        } => {
            let level = view::level_for_outcome(*job_outcome);
            match (job_outcome.is_success(), parsed) {
                (true, Ok(PeekReport::Overview { items })) => html! {
                    @if level != components::Level::Ok {
                        (view::outcome_notice(lang, *job_outcome, &registry.mask(stderr)))
                    }
                    (view::overview_body(lang, profile, items, |ns| namespace_href(profile, ns)))
                },
                (true, Ok(PeekReport::Namespace { ns, items })) => html! {
                    @if level != components::Level::Ok {
                        (view::outcome_notice(lang, *job_outcome, &registry.mask(stderr)))
                    }
                    (view::namespace_body(
                        lang,
                        profile,
                        ns,
                        limit,
                        items,
                        revealed,
                        registry,
                        PEEK_REVEAL_PATH,
                        &namespace_href(profile, ns),
                    ))
                },
                (true, Err(e)) => html! {
                    (components::page_head(PEEK_TITLE, None))
                    (components::notice(components::Level::Error, &e.explain(lang), html! {
                        @let masked_stderr = registry.mask(stderr);
                        @if !masked_stderr.is_empty() {
                            pre class="logdump" { (masked_stderr) }
                        }
                    }))
                },
                (false, _) => html! {
                    (components::page_head(PEEK_TITLE, None))
                    (view::outcome_notice(lang, *job_outcome, &registry.mask(stderr)))
                },
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /peek` — 프로파일 선택, 이후 개요/상세를 마스킹해서 보여준다. 항상 원문을 보여줄
/// 수 없는 경로다(모듈 헤더 참조).
pub async fn page(
    State(ctx): State<Arc<ServeConfig>>,
    Extension(auth): Extension<Arc<AuthState>>,
    uri: Uri,
) -> Response {
    let params = Params::from_query(&uri);
    let registry = request_registry(&ctx, &auth);

    let Some(selection) = selection_from(&params) else {
        let profiles = load_profile_names(&ctx);
        let body = view::picker_body(ctx.lang, &profiles, None);
        return no_store(
            (StatusCode::OK, layout::shell(ctx.lang, PEEK_TITLE, body)).into_response(),
        );
    };

    let spec = match peek_spec(&selection, ctx.lang) {
        Ok(spec) => spec,
        Err(e) => {
            let profiles = load_profile_names(&ctx);
            let notice = components::notice(
                components::Level::Error,
                ctx.lang.sel("Invalid selection", "잘못된 선택"),
                html! { p { (e.to_string()) } },
            );
            let body = view::picker_body(ctx.lang, &profiles, Some(notice));
            return no_store(
                (
                    StatusCode::BAD_REQUEST,
                    layout::shell(ctx.lang, PEEK_TITLE, body),
                )
                    .into_response(),
            );
        }
    };

    let outcome = collect(&ctx.jobs, &spec).await;
    let body = render(
        ctx.lang,
        &selection.profile,
        selection.limit,
        &outcome,
        false,
        &registry,
    );
    no_store((StatusCode::OK, layout::shell(ctx.lang, PEEK_TITLE, body)).into_response())
}

/// 원문 노출 이벤트를 감사 로그에 남긴다. **이 함수가 성공해야만** 호출부가 원문을
/// 렌더할 수 있다 — 실패를 삼키지 않고 그대로 전파한다(태스크 지침, 모듈 헤더 `record()`
/// 절). `target`은 "누가·언제"는 `AuditEvent`의 다른 필드가 채우므로 여기서는 "어느
/// 네임스페이스"만 특정하면 되는데, 프로파일이 다르면 같은 이름의 네임스페이스도 서로
/// 다른 DB를 가리키므로 `profile:ns` 형태로 둘 다 남긴다.
async fn record_reveal(ctx: &ServeConfig, profile: &str, ns: &str) -> Result<()> {
    let target = format!("{profile}:{ns}");
    ctx.audit
        .record(AuditEvent {
            actor: AUDIT_ACTOR,
            action: AUDIT_ACTION_REVEAL,
            target: &target,
            args_masked: &[],
            outcome: AuditOutcome::Success,
            exit_code: None,
        })
        .await
}

/// `POST /peek/reveal` — 원문 노출. `ns`가 없으면 거부한다(모듈 헤더 "개요에서는 원문
/// 노출을 허용하지 않는다" 절). 감사 기록이 성공했을 때만 원문을 그린다.
pub async fn reveal(
    State(ctx): State<Arc<ServeConfig>>,
    Extension(auth): Extension<Arc<AuthState>>,
    body: String,
) -> Response {
    let params = Params::from_body(&body);
    let registry = request_registry(&ctx, &auth);

    let profile = match params.get(FIELD_PROFILE) {
        Some(p) => p.to_string(),
        None => {
            return error_page(
                ctx.lang,
                StatusCode::BAD_REQUEST,
                components::notice(
                    components::Level::Error,
                    ctx.lang.sel("Missing profile", "프로파일 누락"),
                    html! { p { (ctx.lang.sel("A profile is required.", "프로파일이 필요합니다.")) } },
                ),
            );
        }
    };
    let Some(ns) = params.get(FIELD_NS) else {
        return error_page(
            ctx.lang,
            StatusCode::BAD_REQUEST,
            components::notice(
                components::Level::Error,
                ctx.lang.sel("Missing namespace", "네임스페이스 누락"),
                html! {
                    p { (ctx.lang.sel(
                        "Revealing raw values requires a single namespace — the overview cannot be revealed as a whole.",
                        "원문 노출에는 네임스페이스 하나가 필요합니다 — 개요 전체는 한 번에 노출할 수 없습니다.",
                    )) }
                },
            ),
        );
    };
    let ns = ns.to_string();
    let selection = Selection {
        profile: profile.clone(),
        ns: Some(ns.clone()),
        limit: parse_limit(&params),
    };

    let spec = match peek_spec(&selection, ctx.lang) {
        Ok(spec) => spec,
        Err(e) => {
            return error_page(
                ctx.lang,
                StatusCode::BAD_REQUEST,
                components::notice(
                    components::Level::Error,
                    ctx.lang.sel("Invalid selection", "잘못된 선택"),
                    html! { p { (e.to_string()) } },
                ),
            );
        }
    };

    let outcome = collect(&ctx.jobs, &spec).await;

    // 실제로 문서를 보여줄 수 있는 경우에만 감사 기록 → 원문 렌더로 이어진다. 그 외에는
    // (실행 실패·파싱 실패·개요로 조용히 응답이 바뀐 경우 등) 아무것도 노출되지 않았으므로
    // 기록도 남기지 않는다 — 노출되지 않은 일을 "노출됨"으로 기록하면 그 자체가 거짓
    // 감사 로그다.
    let can_reveal = matches!(
        &outcome,
        Outcome::Ran { outcome, parsed: Ok(PeekReport::Namespace { .. }), .. }
            if outcome.is_success()
    );

    if can_reveal {
        if let Err(e) = record_reveal(&ctx, &profile, &ns).await {
            return error_page(
                ctx.lang,
                StatusCode::INTERNAL_SERVER_ERROR,
                components::notice(
                    components::Level::Error,
                    ctx.lang
                        .sel("Audit log write failed", "감사 로그 기록 실패"),
                    html! {
                        p { (ctx.lang.sel(
                            "Raw values were not shown because the audit log could not be written.",
                            "감사 로그를 기록하지 못해 원문을 표시하지 않았습니다.",
                        )) }
                        p class="muted" { (e.to_string()) }
                    },
                ),
            );
        }
    }

    let body = render(
        ctx.lang,
        &profile,
        selection.limit,
        &outcome,
        can_reveal,
        &registry,
    );
    no_store((StatusCode::OK, layout::shell(ctx.lang, PEEK_TITLE, body)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::{self, AuthState};
    use axum::body::Body;
    use axum::http::{header as http_header, Request};
    use axum::middleware;
    use axum::routing::{get, post};
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token-peek-9f2c1a";

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

    /// 이 화면만 담은 최소 라우터 — `server.rs::router`와 같은 순서로 조립한다.
    ///
    /// 인증은 `route_layer`(매칭된 경로에만), `Extension<Arc<AuthState>>`는 그 바깥의
    /// `layer`다. 뒤집으면 Extension이 인증 미들웨어보다 안쪽에 들어가 핸들러가 그것을
    /// 못 뽑고 500이 된다(`routes::dashboard`의 같은 함수에 적힌 함정과 동일).
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(PEEK_PATH, get(page))
            .route(PEEK_REVEAL_PATH, post(reveal))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .layer(Extension(Arc::clone(&auth_and_session().0)))
            .with_state(ctx)
    }

    async fn get_req(
        ctx: Arc<ServeConfig>,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, Vec<(String, String)>, String) {
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
        let headers = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    // ---- 인증 ----

    /// 인증 없이는 401, 쿠키가 있으면 200 — 프로파일을 고르기 전(피커)에는 자식을
    /// 띄우지 않으므로 DB 없이도 이 경로가 검증된다.
    #[tokio::test]
    async fn page_requires_auth_then_renders_picker() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, _headers, body) = get_req(Arc::clone(&ctx), PEEK_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.contains(PEEK_TITLE), "미인증 응답이 화면을 흘렸다");

        let (status, _headers, body) = get_req(ctx, PEEK_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(PEEK_TITLE));
    }

    /// 마스킹 응답(피커 화면)에도 `Cache-Control: no-store`가 붙는다 — 원문 응답만이
    /// 아니라 전 경로에 적용된다(모듈 헤더 "적용 범위" 절).
    #[tokio::test]
    async fn masked_response_carries_no_store() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, headers, _body) = get_req(ctx, PEEK_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        let cache_control = headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("cache-control"))
            .map(|(_, v)| v.as_str());
        assert_eq!(cache_control, Some("no-store"), "no-store 헤더 누락");
    }

    /// `POST /peek/reveal`도 인증을 요구한다.
    #[tokio::test]
    async fn reveal_requires_auth() {
        let ctx = Arc::new(ServeConfig::for_test());
        let response = router(ctx)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PEEK_REVEAL_PATH)
                    .header(
                        http_header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(Body::from("profile=p&ns=db.c"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    }

    /// `ns` 없이 원문 노출을 요청하면 거부된다 — 개요 전체를 한 번에 노출할 수 없다.
    #[tokio::test]
    async fn reveal_without_namespace_is_rejected() {
        let ctx = Arc::new(ServeConfig::for_test());
        let response = router(ctx)
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(PEEK_REVEAL_PATH)
                    .header(
                        http_header::COOKIE,
                        format!("{}={}", auth::SESSION_COOKIE_NAME, session()),
                    )
                    .header(
                        http_header::CONTENT_TYPE,
                        "application/x-www-form-urlencoded",
                    )
                    .body(Body::from("profile=p"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    // ---- 쿼리 파싱 ----

    #[test]
    fn params_decode_percent_and_plus() {
        let uri: Uri = "/peek?profile=a%2Bb&ns=db.c%20d".parse().unwrap();
        let params = Params::from_query(&uri);
        assert_eq!(params.get("profile"), Some("a+b"));
        assert_eq!(params.get("ns"), Some("db.c d"));
    }

    #[test]
    fn empty_query_yields_no_selection() {
        let uri: Uri = "/peek".parse().unwrap();
        let params = Params::from_query(&uri);
        assert!(selection_from(&params).is_none());
    }

    #[test]
    fn blank_profile_value_is_treated_as_absent() {
        let uri: Uri = "/peek?profile=".parse().unwrap();
        let params = Params::from_query(&uri);
        assert!(selection_from(&params).is_none());
    }

    #[test]
    fn limit_out_of_range_is_clamped() {
        let uri: Uri = "/peek?profile=p&ns=db.c&limit=99999".parse().unwrap();
        let params = Params::from_query(&uri);
        assert_eq!(parse_limit(&params), MAX_LIMIT);

        let uri: Uri = "/peek?profile=p&limit=not-a-number".parse().unwrap();
        let params = Params::from_query(&uri);
        assert_eq!(parse_limit(&params), DEFAULT_LIMIT);
    }

    // ---- JobSpec 조립: 셸 비경유, 옵션 주입 거부 ----

    #[test]
    fn valid_selection_builds_expected_argv() {
        let selection = Selection {
            profile: "prod".to_string(),
            ns: Some("app.users".to_string()),
            limit: 7,
        };
        let spec = peek_spec(&selection, Lang::En).unwrap();
        let argv = spec.to_argv();
        assert_eq!(argv[0], "peek");
        assert!(argv.contains(&"--profile".to_string()));
        assert!(argv.contains(&"prod".to_string()));
        assert!(argv.contains(&"--ns".to_string()));
        assert!(argv.contains(&"app.users".to_string()));
        assert!(argv.contains(&"--limit".to_string()));
        assert!(argv.contains(&"7".to_string()));
        assert_eq!(argv.last().unwrap(), "--json");
    }

    #[test]
    fn overview_selection_has_no_ns_or_limit_flag() {
        let selection = Selection {
            profile: "prod".to_string(),
            ns: None,
            limit: DEFAULT_LIMIT,
        };
        let argv = peek_spec(&selection, Lang::En).unwrap().to_argv();
        assert!(!argv.contains(&"--ns".to_string()));
        assert!(!argv.contains(&"--limit".to_string()));
    }

    /// 옵션 주입·경로 탈출 형태의 프로파일/네임스페이스는 명세 조립 단계에서 거부된다
    /// (`job::args`의 화이트리스트를 그대로 물려받는다).
    #[test]
    fn hostile_selection_values_are_rejected() {
        for profile in ["--force", "../etc", "a;rm -rf /"] {
            let selection = Selection {
                profile: profile.to_string(),
                ns: None,
                limit: DEFAULT_LIMIT,
            };
            assert!(
                peek_spec(&selection, Lang::En).is_err(),
                "'{profile}'이 통과했다"
            );
        }
        let selection = Selection {
            profile: "prod".to_string(),
            ns: Some("nodot".to_string()),
            limit: DEFAULT_LIMIT,
        };
        assert!(
            peek_spec(&selection, Lang::En).is_err(),
            "점 없는 ns가 통과했다"
        );
    }

    // ---- 원문 사전 깊이 스캔 ----
    //
    // 스캐너 자체의 성질(문자열 리터럴 무시·이스케이프 추적·비재귀)은
    // `crate::web::jsonguard`의 단위 테스트가 고정한다. 여기서는 **이 화면이 그 관문을
    // 실제로 통과시키는지**만 본다 — 관문이 아무리 정확해도 부르지 않으면 소용없다.

    #[test]
    fn parse_report_rejects_too_deep_input_before_parsing() {
        // 바깥 객체(1) + `documents` 배열(1) + 중첩 200겹 = 202.
        let deep_ns_array = format!(
            r#"{{"schema":1,"ns":"db.c","documents":[{}{}]}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        assert_eq!(
            parse_report(&deep_ns_array).unwrap_err(),
            ReportError::TooDeep {
                found: 202,
                max: jsonguard::MAX_JSON_DEPTH,
            }
        );
    }

    /// 상한 **안쪽**의 중첩은 그대로 파싱된다 — 관문이 정상 문서를 막지 않는다는 반대편
    /// 고정. 이것이 없으면 상한을 1로 낮춰도 위 테스트는 통과한다.
    #[test]
    fn parse_report_accepts_nesting_within_the_limit() {
        let nested = format!(
            r#"{{"schema":1,"ns":"db.c","documents":[{{"a":{}{}}}]}}"#,
            "[".repeat(50),
            "]".repeat(50)
        );
        assert!(
            parse_report(&nested).is_ok(),
            "상한 안쪽 문서가 거부됐다: {:?}",
            parse_report(&nested).unwrap_err()
        );
    }

    // ---- 파싱: 실제 CLI 출력 표본 ----

    /// `src/cli/handlers/peek.rs::overview_json`이 실제로 내는 모양(수기 표본이지만 그
    /// 함수의 필드 이름·중첩을 그대로 옮겼다).
    #[test]
    fn real_overview_output_parses() {
        let sample = r#"{"schema":1,"namespaces":[
            {"ns":"app.users","count":42,"latest":{"_id":{"$oid":"507f1f77bcf86cd799439011"},"email":"a@b.com"}},
            {"ns":"app.empty","count":0,"latest":null}
        ]}"#;
        let report = parse_report(sample).expect("표본이 파싱되지 않는다");
        let PeekReport::Overview { items } = report else {
            panic!("Overview로 파싱돼야 함");
        };
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].ns, "app.users");
        assert!(items[0].latest.is_some());
        assert!(
            items[1].latest.is_none(),
            "null latest는 None으로 접혀야 함"
        );
    }

    /// `documents_json`(Mongo) 모양.
    #[test]
    fn real_namespace_documents_output_parses() {
        let sample = r#"{"schema":1,"ns":"app.users","documents":[{"_id":1,"name":"a"}]}"#;
        let report = parse_report(sample).expect("파싱 실패");
        let PeekReport::Namespace { ns, items } = report else {
            panic!("Namespace로 파싱돼야 함");
        };
        assert_eq!(ns, "app.users");
        assert_eq!(items.len(), 1);
    }

    /// `rows_json`(PG/MySQL) 모양 — 키가 `rows`라는 것만 다르고 나머지는 같은 취급이다.
    #[test]
    fn real_namespace_rows_output_parses() {
        let sample = r#"{"schema":1,"ns":"public.t","rows":[{"id":1}]}"#;
        let report = parse_report(sample).expect("파싱 실패");
        let PeekReport::Namespace { ns, items } = report else {
            panic!("Namespace로 파싱돼야 함");
        };
        assert_eq!(ns, "public.t");
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn broken_stdout_becomes_readable_errors() {
        assert_eq!(parse_report("").unwrap_err(), ReportError::Empty);
        assert_eq!(parse_report("   \n\t ").unwrap_err(), ReportError::Empty);
        assert!(matches!(
            parse_report("not json"),
            Err(ReportError::Malformed(_))
        ));
        assert!(matches!(parse_report("{}"), Err(ReportError::Malformed(_))));
        assert_eq!(
            parse_report(r#"{"schema":99,"namespaces":[]}"#).unwrap_err(),
            ReportError::SchemaMismatch {
                found: 99,
                expected: EXPECTED_PEEK_SCHEMA
            }
        );
    }

    // ---- exit 4/5는 실패가 아니다 ----

    /// exit 4(경고 동반 성공)는 데이터가 정상적으로 그려지고, exit 5(락 충돌)는 실패가
    /// 아니라 재시도 가능한 경고로 접힌다 — 둘 다 `JobOutcome`이 이미 보장하는 성질이고,
    /// 이 화면의 레벨 매핑([`view::level_for_outcome`])이 그것을 뒤집지 않는지 확인한다.
    #[test]
    fn exit_four_and_five_are_not_rendered_as_failure() {
        assert_eq!(
            view::level_for_outcome(JobOutcome::SucceededWithWarnings),
            components::Level::Warn
        );
        assert_ne!(
            view::level_for_outcome(JobOutcome::SucceededWithWarnings),
            components::Level::Fail
        );
        assert_eq!(
            view::level_for_outcome(JobOutcome::LockConflict),
            components::Level::Warn
        );
        assert_ne!(
            view::level_for_outcome(JobOutcome::LockConflict),
            components::Level::Fail
        );

        let outcome = Outcome::Ran {
            outcome: JobOutcome::SucceededWithWarnings,
            parsed: Ok(PeekReport::Overview { items: vec![] }),
            stderr: String::new(),
        };
        let out = render(
            Lang::En,
            "prod",
            DEFAULT_LIMIT,
            &outcome,
            false,
            &SecretRegistry::new(),
        )
        .into_string();
        assert!(
            !out.to_lowercase().contains("failed"),
            "경고가 실패로 표기됐다: {out}"
        );
    }

    // ---- 감사 기록 ----

    /// 원문 노출 이벤트가 감사 로그에 실제로 남는다(누가·언제·어느 네임스페이스).
    #[tokio::test]
    async fn record_reveal_writes_an_audit_line() {
        let ctx = ServeConfig::for_test();
        record_reveal(&ctx, "prod", "app.users").await.unwrap();
        let content = std::fs::read_to_string(ctx.audit.path()).unwrap();
        let line: serde_json::Value =
            serde_json::from_str(content.lines().next().unwrap()).unwrap();
        assert_eq!(line["actor"], AUDIT_ACTOR);
        assert_eq!(line["action"], AUDIT_ACTION_REVEAL);
        assert_eq!(line["target"], "prod:app.users");
        assert_eq!(line["outcome"], "success");
    }

    // ---- 링크 인코딩 ----

    #[test]
    fn href_helpers_produce_safe_query_strings() {
        assert_eq!(overview_href("prod"), "/peek?profile=prod");
        assert_eq!(
            namespace_href("prod", "app.users"),
            "/peek?profile=prod&ns=app.users"
        );
        // 방어적 인코딩 — 화이트리스트를 통과한 값에는 실질적 영향이 없지만 특수문자가
        // 섞여도 안전한 쿼리를 낸다.
        assert_eq!(url_encode("a b"), "a%20b");
        assert_eq!(url_encode("a&b=c"), "a%26b%3Dc");
    }
}
