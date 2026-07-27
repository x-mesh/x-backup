//! `GET /dashboard` — 전 프로파일 상태를 한 화면에(R11).
//!
//! ## 이 화면이 답하는 질문은 하나다: "지금 괜찮은가"
//! 운영자가 터미널을 열지 않고 그 판단을 내릴 수 있어야 한다. 그래서 보여주는 것이
//! 프로파일마다 여섯 가지로 고정된다 — 연결·토폴로지·복제 지연·마지막 백업 나이·
//! destination 쓰기 가능/여유 공간, 그리고 프로브 왕복 시간.
//!
//! 여섯 개를 고른 기준: **하나라도 나쁘면 백업이 오늘 밤 실패하거나, 이미 실패했는데
//! 아무도 모르는 상태**다. 연결이 안 되면 못 뜬다. destination이 못 쓰면 못 쓴다.
//! 마지막 백업이 이틀 전이면 이미 실패한 것이다. 그 외의 정보(문서 수·인덱스 크기)는
//! 이 판단에 기여하지 않으므로 이 화면에 없다 — `/doctor`와 프로파일 상세의 몫이다.
//!
//! ## 왜 `status --all --json`을 쓰지 않는가 — **부분 실패가 불가능하다**
//! 데이터 원천으로 `status --all`이 가장 자연스러워 보이지만 쓸 수 없다. 실측 근거:
//!
//! 1. **한 프로파일이 전체를 끊는다.** `src/cli/handlers/status.rs::handle_all`은
//!    `for name in &names { reports.push(build_report(...)?) }` 형태다 — `?`가 붙어 있다.
//!    `source.uri`/`uri_env`가 빠진 프로파일 하나만 있어도 `build_report`가 `Err`를
//!    내고 **전체 명령이 끊겨 stdout에 아무 보고서도 나오지 않는다.** 프로파일 50개 중
//!    49개가 멀쩡해도 화면에 행이 0개가 된다. 이 화면의 존재 이유와 정면으로 충돌한다.
//! 2. **직렬 실행이다.** 같은 루프가 프로파일을 하나씩 `await`한다. 50개면 DB 연결
//!    50번이 줄지어 일어나므로 화면 지연이 프로파일 수에 비례해 커진다.
//! 3. **프로파일별 상한을 걸 수 없다.** 자식이 하나이므로 상한도 하나다. 멈춘 프로파일
//!    하나가 곧 멈춘 화면이다.
//!
//! 그래서 이 화면은 **프로파일마다 자식을 하나씩** 띄운다(`status --profile <name> --json`).
//! 상한도 프로파일마다 따로 걸리고, 하나가 죽어도 그 행만 진단으로 바뀐다.
//!
//! ## 명부(roster)는 `doctor --json`에서 얻는다
//! 프로파일별로 띄우려면 먼저 프로파일 목록이 필요하다. 그 목록을 얻는 방법으로
//! `doctor --json`을 고른 이유:
//!
//! - **오프라인이다.** `doctor`는 config 파싱 + 몇 번의 `stat()`뿐이고 DB에 연결하지
//!   않는다(`src/cli/handlers/doctor.rs` 헤더). 목록을 얻는 값으로 프로덕션에 연결이
//!   생기지 않는다.
//! - **엔진 라벨을 함께 준다**(`profiles[].db`). 행의 `Engine` 칸이 그것이다 —
//!   `status --json`에는 엔진 필드가 없어서, 이것 없이는 엔진을 보여주려고 웹이 URI를
//!   해석해야 한다(= 도메인 로직 재구현, 최상위 불변식 위반).
//! - **웹이 config를 파싱하지 않는다.** config TOML을 우리가 읽어 프로파일 목록을
//!   뽑는 방법도 있지만, 그러면 `default_profile`·상속 규칙 해석을 웹이 이중 구현하기
//!   시작한다. 자식에게 묻는 편이 규약에 맞다.
//!
//! ## ⚠ 함정 — `status --json`은 **여러 줄 pretty**이고 `schema`가 마지막 키다
//! 같은 명령이 플래그에 따라 직렬화 형식이 갈린다:
//!
//! | 호출 | 형식 | 근거 |
//! |---|---|---|
//! | `status --json` (단일) | 여러 줄 pretty | `render_json`이 `to_string_pretty`를 쓴다 |
//! | `status --all --json` | 한 줄 compact | `println!("{}", profiles_json(items))`(Display) |
//!
//! 이것이 함정인 이유는 [`crate::web::job::stream::classify_stdout_line`]이 **한 줄**을
//! 파싱해 `schema` 키의 유무로 "이 줄이 최종 요약인가"를 판별하기 때문이다. `status`를
//! `relay_stdout` 스트리밍 경로로 흘리면 pretty 출력의 어느 한 줄도 완전한 JSON이 아니어서
//! 요약이 전부 `Unrecognized`로 격하된다.
//!
//! 이 화면은 그 경로를 **아예 타지 않는다**: [`RunningJob::wait_with_output`](crate::web::job::RunningJob::wait_with_output)
//! 으로 stdout 전체를 받아 통째로 파싱한다. `status`는 몇백 ms에 끝나는 짧은 명령이라
//! 진행률 스트리밍이 필요 없고, 통째로 받으면 pretty든 compact든 상관이 없다.
//!
//! **스트리밍 경로를 쓰고 싶어진다면 선행 조건이 있다**: `status --json`의 직렬화를 한 줄로
//! 바꾸는 것(`to_string_pretty` → `to_string`)이 먼저다. 그 파일(`src/cli/handlers/status.rs`)은
//! 이 태스크의 소유가 아니므로 여기서 고치지 않는다 — CLI 출력 형식 변경은 사람이 읽는
//! `--json`을 쓰는 기존 사용자에게 영향이 가는 결정이고, 웹 편의를 위해 조용히 바꿀 일이 아니다.
//!
//! ## 프로파일별 상한 + 전체 예산 — 하나가 전체를 멈추지 못하게
//! [`ProbeLimits`]에 값과 근거를 적었다. 요점 세 가지:
//! - 프로브마다 상한이 있고(초과 시 `SIGKILL`), 초과한 프로파일은 그 행만 진단이 된다.
//! - 동시 실행 폭을 제한한다 — 프로파일 50개에 자식 50개를 동시에 띄우면 이 화면이
//!   바로 그 "조용한 지속 부하"의 원인이 된다([`crate::web::cache`] 헤더).
//! - 그래서 배치가 여러 번 돌 수 있으므로 **화면 전체 예산**이 따로 필요하다. 예산을
//!   넘긴 프로파일은 "이번 요청에서 보지 않았다"로 정직하게 표시한다 — 프록시의 504
//!   기본 페이지보다 우리가 만든 부분 화면이 낫다.
//!
//! ## 자동 갱신은 넣지 않는다
//! 근거는 세 가지이고, 셋 다 "뷰어가 없으면 폴링이 멈춰야 한다"는 요구에서 나온다:
//!
//! 1. **멈출 수 없는 폴링을 만들지 않는다.** JS `setInterval`은 탭이 열려 있는 동안
//!    계속 돈다. `visibilitychange`로 백그라운드 탭은 멈출 수 있지만, **잊고 열어 둔
//!    포그라운드 탭**은 밤새 프로덕션 DB를 두드린다. 서버가 뷰어 수를 세는 방식
//!    (SSE 연결 카운트)이면 정확하지만, 그건 이 화면이 아니라 t11 계층의 일이다.
//! 2. **`layout.rs`에 htmx가 없고 그 파일은 이 태스크의 소유가 아니다.** 내 본문
//!    마크업에서 `<script>`를 내보내면 레이아웃의 에셋 정책(외부 리소스 0개·지문 URL)을
//!    화면 하나가 우회하는 셈이 된다.
//! 3. **수동 새로고침 + TTL 캐시면 부하 상한이 계산 가능하다**: 프로파일당 자식은
//!    TTL당 최대 하나다([`crate::web::cache::PROBE_TTL`]). 새로고침을 연타해도 그 상한을
//!    넘지 않는다. 자동 갱신을 넣으면 상한이 "뷰어 수 × 갱신 주기"가 되고, 뷰어 수를
//!    모르면 상한도 모른다.
//!
//! 대신 화면이 **데이터 나이를 표시한다**(행마다 `Age`, 상단에 프로브 총 소요). 낡은
//! 값을 낡은 줄 모르고 보는 것이 자동 갱신 부재보다 훨씬 위험하다.
//!
//! ## 인증 뒤 화면이다
//! 프로파일 이름·엔진·destination 위치·연결 상태를 한 화면에 모은다 — `/doctor`가
//! config 구조를 드러내는 것과 같은 종류의 지도이고, 밀도는 더 높다.
//!
//! ## 시크릿
//! 두 겹으로 지운다:
//! 1. [`request_registry`] — 잡 러너가 자식에게 실제로 주입하는 값들 + 세션 토큰
//!    (`routes::jobs`·`routes::backup`·`routes::doctor`와 같은 방식).
//! 2. [`redact_uris`] — **레지스트리가 모르는 URI**를 접는다. config에 `uri`를 인라인으로
//!    적은 프로파일의 접속 문자열은 env를 거치지 않으므로 [`crate::web::job::JobSecrets`]에
//!    등록되지 않는다. 그런데 `status`의 연결 실패 메시지는 드라이버 오류를 그대로 싣고,
//!    드라이버 오류에는 접속 문자열이 들어올 수 있다 — 그 경로가 이 화면에서 유일하게
//!    비밀번호가 새어 나올 수 있는 자리였다.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Extension, State};
use maud::Markup;
use serde::Deserialize;

use crate::cli::handlers::status::STATUS_JSON_SCHEMA;
use crate::i18n::Lang;
use crate::web::auth::AuthState;
use crate::web::cache::{self, ProbeKey, ProbeOutcome, ProbeOutput, TtlCache};
use crate::web::job::exit as job_exit;
use crate::web::job::{JobCommand, JobOutcome, JobRunner, JobSpec, ProfileName, RunningJob};
use crate::web::jsonguard;
use crate::web::mask::SecretRegistry;
use crate::web::routes::doctor;
use crate::web::view::components::Level;
use crate::web::view::{dashboard as view, layout};
use crate::web::ServeConfig;

/// 화면 경로. 라우터·마크업·테스트가 이 상수를 공유한다.
///
/// `/`를 쓰지 않는다 — 그 자리는 현재 콘솔 랜딩이고, 무엇이 첫 화면인지는 라우터 배선
/// 사안이다([`crate::web::server::app_routes`]). 이 태스크가 그 결정을 대신 내리지 않는다.
pub const DASHBOARD_PATH: &str = "/dashboard";

/// `<title>`·화면 제목·내비게이션 라벨이 공유하는 이름. 기술용어이므로 영문 고정
/// ([`crate::i18n`] 규약). [`layout`]이 현재 화면 판정에 이 값을 쓴다.
pub const DASHBOARD_TITLE: &str = "Dashboard";

/// 진단 텍스트(자식 stderr·파서 메시지) 발췌 상한(문자 수).
///
/// 대시보드는 행이 프로파일 수만큼 있으므로 doctor 화면(400자)보다 짧게 잡는다 —
/// 프로파일 50개가 각자 400자를 물고 있으면 진단이 화면을 삼킨다.
const EXCERPT_CHARS: usize = 240;

// ---------------------------------------------------------------------------
// 실행 상한 — 값과 근거
// ---------------------------------------------------------------------------

/// 프로브 실행 상한 묶음.
///
/// 상수를 그대로 쓰지 않고 구조체로 받는 이유는 테스트다. 실제 값(초 단위)으로
/// 타임아웃·예산 동작을 검증하면 테스트 하나가 수십 초 걸린다. 값이 아니라
/// **메커니즘**을 검증해야 하므로 밀리초 단위로 줄여 주입한다
/// (`routes::doctor`의 `slow_child_hits_the_timeout`과 같은 판단).
#[derive(Debug, Clone, Copy)]
pub struct ProbeLimits {
    /// 명부(`doctor --json`) 한 번의 상한.
    pub roster: Duration,
    /// 프로파일 하나(`status --profile … --json`)의 상한.
    pub profile: Duration,
    /// 화면 전체가 프로브에 쓸 수 있는 총 시간.
    pub budget: Duration,
    /// 동시에 띄우는 프로파일 프로브 수.
    pub concurrency: usize,
}

impl Default for ProbeLimits {
    /// 운영 기본값 — 각 값의 근거는 아래 주석에 하나씩.
    fn default() -> Self {
        Self {
            // ## 명부 10초
            // `doctor`는 config 파싱 + `stat()`뿐이라 로컬에서 수십 ms에 끝난다. 그럼에도
            // 0이 아닌 상한이 필요한 이유는 `recipient_file`이 NFS·SMB처럼 멈출 수 있는
            // 마운트에 있을 때 `Path::exists()`가 실제로 오래 매달릴 수 있다는 것이다
            // (`routes::doctor::DOCTOR_TIMEOUT`의 같은 근거). 그쪽은 20초지만 여기는
            // 절반이다 — 명부는 화면의 **전제**여서, 명부를 기다리는 시간은 프로파일
            // 프로브가 시작조차 못 하는 시간이다. 명부 10초 + 예산 12초 = 최악 22초로,
            // 앞단 프록시 기본 읽기 상한(nginx 60초)보다 넉넉히 짧다.
            roster: Duration::from_secs(10),
            // ## 프로파일 8초
            // 아래 경계: `status` 한 번은 DB 연결 + 서버 메타 질의 + destination 쓰기
            // 프로브 + manifest 읽기다. 로컬 관측으로 수십~수백 ms이고, 원격 DB·S3
            // destination이면 초 단위까지 정상 범위다. 상한이 그 정상 범위에 걸치면
            // 화면이 멀쩡한 프로파일을 "점검 불가"로 오진한다 — 초록을 빨강으로 만드는
            // 오진이 가장 나쁜 종류다(운영자가 없는 장애를 쫓는다).
            // 위 경계: 사람이 화면 앞에서 기다리는 한계와 프록시 상한. 8초면 원격
            // destination의 정상 지연을 충분히 덮고, 멈춘 대상은 8초에 끊는다.
            profile: Duration::from_secs(8),
            // ## 전체 예산 12초
            // 동시 폭이 8이므로 프로파일 50개는 배치 7번이다. 배치마다 최악 8초면
            // 56초 — 프록시 상한과 부딪힌다. 그래서 프로파일별 상한과 **별개로** 화면
            // 전체 예산이 필요하다. 12초는 (a) 정상 프로파일 50개(배치당 수백 ms)를
            // 전부 덮고, (b) 병목 프로파일이 여러 개여도 화면이 돌아오고, (c) 명부
            // 10초를 더해도 프록시 상한의 절반에 못 미치는 값이다.
            // 예산을 넘긴 프로파일은 실패가 아니라 **미측정**으로 표시한다.
            budget: Duration::from_secs(12),
            // ## 동시 8
            // 프로브 하나가 DB 연결 하나 + destination 쓰기 하나다. 50개를 동시에
            // 띄우면 새로고침 한 번이 프로덕션에 50연결이고, 이 화면이 곧 부하의 원인이
            // 된다([`crate::web::cache`] 헤더가 막으려는 것). 8은 (a) 흔한 규모(프로파일
            // 5~20개)를 한두 배치에 끝내고, (b) 어떤 DB에도 부담이 아닌 연결 수이며,
            // (c) 자식 8개의 메모리·fd가 상주 서버에 무해한 수준이다.
            concurrency: 8,
        }
    }
}

// ---------------------------------------------------------------------------
// 파싱 — `status --json` 문서 (순수)
// ---------------------------------------------------------------------------

/// `status --profile <name> --json`의 최상위 문서.
///
/// 필드 이름은 `crate::engine::mongo::status::StatusReport`의 serde 표현과 1:1이다
/// (거기에 `schema`가 최상위로 덧붙는다 — `status.rs::report_json`). 한쪽만 바뀌면
/// [`StatusError::Malformed`]로 즉시 드러난다.
///
/// `--ns-detail`이 붙으면 `namespaces` 키가 더 붙지만 이 화면은 그 플래그를 쓰지 않고,
/// serde는 모르는 키를 무시하므로 미래에 필드가 늘어도 이 파서는 살아 있다.
#[derive(Debug, Deserialize)]
pub struct StatusDoc {
    /// 스키마 버전. [`parse_status`]가 [`STATUS_JSON_SCHEMA`]와 먼저 대조한다.
    pub schema: u32,
    /// 점검한 프로파일 이름(자식이 실효 프로파일로 해석한 이름).
    pub profile: String,
    /// 자식이 스스로 매긴 전체 신호등(`ok`/`warn`/`fail`). 행의 레벨은 항목에서
    /// 계산하고, 이 값은 **교차 확인용**으로 함께 표시한다.
    pub overall: String,
    /// 점검 항목.
    pub items: Vec<StatusItem>,
}

/// 점검 항목 하나.
#[derive(Debug, Deserialize)]
pub struct StatusItem {
    /// 머신 판독용 안정 키(`connection`/`topology`/`last_backup`/…). 이 화면이 칸을
    /// 고르는 축이다([`CELL_KEYS`]).
    pub key: String,
    /// 사람용 라벨. 표 머리글은 우리 것을 쓰므로 진단 표시에만 쓴다.
    pub label: String,
    /// `ok`/`warn`/`fail`. 매핑은 [`doctor::level_from_status`]를 그대로 쓴다 —
    /// 같은 어휘를 두 번 구현하지 않는다.
    pub status: String,
    /// 짧은 비교용 값(`"7.0.35"`, `"3h ago"`, `"OK"`). 없는 항목도 있다.
    #[serde(default)]
    pub value: Option<String>,
    /// 사람이 읽는 상세. **자식이 만든 남의 텍스트**이므로 마스킹·이스케이프 대상이다.
    pub message: String,
}

/// stdout을 [`StatusDoc`]으로 접지 못한 이유.
///
/// `doctor::ReportError`와 변종 모양이 같지만 따로 두었다. 합치지 않은 이유는 설명
/// 문장이 다르기 때문이다 — "doctor가 아무 출력도 내지 않았습니다"와 "status가 …"는
/// 운영자가 의심할 곳이 다르고(전자는 config, 후자는 대상 서버), 그 문구가 두 화면의
/// 유일한 차이다. 공유 타입으로 만들면 명령 이름을 인자로 받는 함수가 되어, 문구를
/// 고칠 때 두 화면 모두를 검토해야 한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StatusError {
    /// stdout이 비었다(자식이 아무것도 못 찍고 끝났다).
    Empty,
    /// JSON이 아니거나 기대한 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 이 빌드가 아는 값과 다르다.
    SchemaMismatch { found: u64, expected: u32 },
    /// 중첩이 너무 깊어 **파싱을 시도하지 않았다**([`crate::web::jsonguard`]).
    ///
    /// 이 화면은 프로파일마다 자식을 띄우므로 이 오류는 **그 행 하나에만** 실린다 —
    /// 나머지 프로파일의 상태는 그대로 그려진다.
    TooDeep { found: usize, max: usize },
}

impl StatusError {
    /// 행의 진단 줄에 그대로 들어가는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            StatusError::Empty => lang
                .sel(
                    "status produced no JSON on stdout. Read the exit code below — the child usually stopped before it could report.",
                    "status가 stdout에 JSON을 내지 않았습니다. 아래 종료 코드를 보세요 — 대개 보고하기 전에 자식이 멈춘 경우입니다.",
                )
                .to_string(),
            StatusError::Malformed(detail) => format!(
                "{} {}",
                lang.sel(
                    "status output could not be parsed as the expected JSON:",
                    "status 출력을 기대한 JSON으로 해석할 수 없습니다:",
                ),
                excerpt(detail)
            ),
            StatusError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} ≠ {expected})",
                lang.sel(
                    "status reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "status가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            StatusError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "status output is nested more deeply than this console parses. Only this profile's row is affected — the others are unaffected.",
                    "status 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다. 이 프로파일 행만 영향을 받고 나머지는 무관합니다.",
                )
            ),
        }
    }
}

/// 자식 stdout을 [`StatusDoc`]으로 접는다 — **순수 함수**.
///
/// **스키마 버전을 본문 역직렬화보다 먼저 본다**(`doctor::parse_report`와 같은 순서).
/// 그래야 스키마 2가 필드를 개편했을 때 "필드가 이상하다"가 아니라 "버전이 다르다"로
/// 진단되어, 운영자가 config가 아니라 배포 버전을 보게 된다.
///
/// 입력이 `&str`인 것에 유의: 이 경로의 stdout은 잡 러너가 이미 lossy 변환을 마친
/// 값이다([`crate::web::job::JobCompletion`]). 즉 "비-UTF-8" 변종이 필요 없다 —
/// 비-UTF-8 바이트는 대체 문자로 바뀌어 [`StatusError::Malformed`]로 접힌다.
/// 종료 코드를 잃지 않기 위한 그쪽 결정을 이 파서가 그대로 물려받는다.
pub fn parse_status(stdout: &str) -> Result<StatusDoc, StatusError> {
    if stdout.trim().is_empty() {
        return Err(StatusError::Empty);
    }
    jsonguard::check_depth(stdout).map_err(|d| StatusError::TooDeep {
        found: d.found,
        max: d.max,
    })?;
    let value: serde_json::Value =
        serde_json::from_str(stdout).map_err(|e| StatusError::Malformed(e.to_string()))?;
    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(found) if found == u64::from(STATUS_JSON_SCHEMA) => {}
        Some(found) => {
            return Err(StatusError::SchemaMismatch {
                found,
                expected: STATUS_JSON_SCHEMA,
            })
        }
        None => {
            return Err(StatusError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }
    serde_json::from_value(value).map_err(|e| StatusError::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------
// 판정 — 항목·종료 코드를 화면 상태로 접는다 (순수)
// ---------------------------------------------------------------------------

/// 레벨의 심각도 순위 — "가장 나쁜 상태"를 고르는 데 쓴다.
///
/// `routes::doctor`에도 같은 함수가 있다(그쪽은 비공개). 공유하려면
/// [`crate::web::view::components`]에 얹어야 하는데 그 파일은 이 브랜치에서 여러
/// 태스크가 함께 쓰는 자리라 손대지 않았다 — **통합 후보로 남긴다**(6줄짜리 중복이
/// 파일 소유권 충돌보다 싸다).
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
fn worst_level(items: &[StatusItem]) -> Level {
    items
        .iter()
        .map(|i| doctor::level_from_status(&i.status))
        .max_by_key(|l| severity(*l))
        .unwrap_or(Level::Error)
}

/// 항목 개수를 레벨별로 센다(`ok`/`warn`/`fail`/해석 불가 순).
fn count_by_level(items: &[StatusItem]) -> [usize; 4] {
    let mut counts = [0usize; 4];
    for item in items {
        counts[usize::from(severity(doctor::level_from_status(&item.status)))] += 1;
    }
    counts
}

/// 종료 코드에 관한 **사실**만 담은 값 — 판정을 다시 하지 않는다.
///
/// `level`은 [`job_exit::level_for_label`]에서 온다. 이 화면이 자기 `match`로 레벨을
/// 정하지 않는 이유는 그 함수 doc에 있다: 사본이 하나도 없으면 갈라질 수 없다. 특히
/// **exit 4(경고 동반 성공)와 exit 5(락 충돌)는 어느 화면에서도 [`Level::Fail`]이 아니다.**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitFacts {
    /// 종료 상태의 화면 레벨.
    pub level: Level,
    /// 결과 라벨(`succeeded`/`precheck-failed`/…). 영문 고정.
    pub label: &'static str,
    /// 종료 코드(시그널 종료·미상은 `None`).
    pub code: Option<i32>,
}

impl ExitFacts {
    /// [`JobOutcome`] 하나에서 사실을 뽑는다.
    pub fn of(outcome: &JobOutcome) -> Self {
        Self {
            level: job_exit::level_for_label(outcome.label()),
            label: outcome.label(),
            code: outcome.exit_code(),
        }
    }

    /// 표에 넣는 짧은 표기(`exit 4 · succeeded-with-warnings`).
    ///
    /// 코드와 라벨을 **둘 다** 적는 이유: 코드만으로는 의미를 외워야 하고, 라벨만으로는
    /// CLI를 손으로 재현할 때 무엇이 나올지 모른다. 시그널 종료는 코드가 없으므로
    /// 라벨만 남는다.
    pub fn text(&self) -> String {
        match self.code {
            Some(code) => format!("exit {code} · {}", self.label),
            None => self.label.to_string(),
        }
    }
}

/// 행의 한 칸 — 값 + 그 값의 상태 + 근거.
#[derive(Debug, Clone)]
pub struct Cell {
    /// 이 칸의 상태. 마크업에는 `data-level` 토큰으로만 나간다.
    pub level: Level,
    /// 칸에 찍히는 짧은 값(`connected`, `3h ago`, `OK`).
    pub text: String,
    /// 왜 그런지 — 자식이 만든 상세 메시지. `text`와 같으면 뷰가 생략한다.
    pub note: String,
}

/// 칸마다 어떤 항목 키를 보는가 — **순서대로 첫 번째로 발견된 것**을 쓴다.
///
/// 후보가 여러 개인 칸(`Replication`)이 있는 이유는 엔진마다 같은 개념을 다른 키로
/// 보고하기 때문이다: PostgreSQL·MySQL은 `replication_lag`, MongoDB는 `secondary`
/// (secondary 가용성)다. 셋을 각각 다른 열로 두면 어느 엔진에서든 열 하나가 통째로
/// 비고, 그러면 표를 위아래로 훑는 눈이 매 행에서 흔들린다.
///
/// 없는 키는 **오류가 아니다** — 엔진에 그 개념이 없을 뿐이므로 칸이 비고([`Option`]),
/// 뷰가 `—`를 그린다. 여기서 [`Level::Error`]로 접으면 정상 PostgreSQL 프로파일이
/// 빨갛게 된다.
const CELL_KEYS: CellKeys = CellKeys {
    connection: &["connection"],
    topology: &["topology"],
    replication: &["replication_lag", "secondary", "replication"],
    last_backup: &["last_backup"],
    destination: &["destination"],
};

/// [`CELL_KEYS`]의 모양 — 칸 이름과 후보 키 목록의 짝.
struct CellKeys {
    connection: &'static [&'static str],
    topology: &'static [&'static str],
    replication: &'static [&'static str],
    last_backup: &'static [&'static str],
    destination: &'static [&'static str],
}

/// 한 행의 칸들. 전부 [`Option`]인 이유는 [`CELL_KEYS`] doc 참조.
#[derive(Debug, Clone)]
pub struct RowCells {
    /// 연결·인증.
    pub connection: Option<Cell>,
    /// 토폴로지(standalone/replica set/…).
    pub topology: Option<Cell>,
    /// 복제 지연 또는 secondary 가용성.
    pub replication: Option<Cell>,
    /// 마지막 백업 나이.
    pub last_backup: Option<Cell>,
    /// destination 쓰기 가능 여부(여유 공간은 `note`에 있다 — 아래 doc).
    pub destination: Option<Cell>,
}

/// 항목 목록에서 칸 하나를 뽑는다.
///
/// `text`는 `value`를 우선하고 없으면 `message`를 발췌한다. `value`가 있는 항목은
/// `status --all` 비교 표에서 쓰이도록 이미 짧게 정제된 값이라(예: `"3h ago"`,
/// `"OK"`, `"wiredTiger"`) 표의 칸에 딱 맞는다.
///
/// **`note`에는 항상 전체 메시지를 담는다.** destination 여유 공간이 그 안에 있기
/// 때문이다: `status --json`은 여유 바이트를 별도 필드로 내주지 않고
/// `"writable (local: /srv/backups), free 12.3 GiB"`처럼 메시지 문장에 넣는다
/// (`status.rs::destination_item`). 그 문장에서 숫자만 뽑아내려면 웹이 사람용 산문을
/// 파싱해야 하고, 그건 로케일·단위 표기가 바뀌는 순간 조용히 깨진다 — 자식이 만든
/// 문장을 그대로 보여주는 편이 정확하고 정직하다.
///
/// **여유 공간을 별도 필드로 노출하는 것이 올바른 해법**이지만 그건 `status --json`
/// 스키마 변경(`src/cli/handlers/status.rs` — 이 태스크의 소유가 아니다)이다.
fn cell_from(items: &[StatusItem], keys: &[&str]) -> Option<Cell> {
    let item = keys
        .iter()
        .find_map(|key| items.iter().find(|i| i.key == *key))?;
    let message = excerpt(&item.message);
    Some(Cell {
        level: doctor::level_from_status(&item.status),
        text: item
            .value
            .as_deref()
            .map_or_else(|| message.clone(), excerpt),
        note: message,
    })
}

/// 문서 하나에서 행의 칸들을 뽑는다 — **순수 함수**.
pub fn cells_of(doc: &StatusDoc) -> RowCells {
    RowCells {
        connection: cell_from(&doc.items, CELL_KEYS.connection),
        topology: cell_from(&doc.items, CELL_KEYS.topology),
        replication: cell_from(&doc.items, CELL_KEYS.replication),
        last_backup: cell_from(&doc.items, CELL_KEYS.last_backup),
        destination: cell_from(&doc.items, CELL_KEYS.destination),
    }
}

/// 행 하나가 화면에 그릴 상태.
///
/// ## `large_enum_variant`을 허용하는 이유
/// `Reported`(369바이트)와 `Unreadable`(128바이트)의 차이를 clippy가 짚는다. 그 조언이
/// 겨냥하는 상황은 "큰 변종이 드문데 모든 값이 그 크기를 떠안는" 경우인데, 여기서는 셋 다
/// 아니다:
///
/// - 이 값은 **프로파일 하나당 하나**만 존재한다. config에 프로파일이 50개여도 50개이고,
///   차이를 다 합쳐도 12KB 남짓이다.
/// - 그중 `Reported`가 **정상 경로**다 — 드문 변종이 아니라 대부분이 그것이다.
/// - 큰 필드([`RowCells`])를 박싱하면 행마다 힙 할당이 하나씩 늘고 렌더 경로에 간접 참조가
///   생긴다. 아끼는 것이 12KB인데 치르는 값이 그것이다.
///
/// 즉 이 자리에서 린트가 가리키는 것은 실제 문제가 아니다. 프로파일 수와 무관하게 이 값을
/// 대량으로 들고 다니는 경로가 생기면 그때 다시 볼 것.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum RowState {
    /// 자식이 끝났고 stdout을 status 문서로 읽었다 — 정상 경로.
    Reported {
        /// 행의 상태 — 항목 중 최악과 종료 코드 레벨 중 **더 나쁜 쪽**([`row_state_from_output`]).
        level: Level,
        /// 자식이 스스로 매긴 신호등(교차 확인용).
        overall: String,
        /// 종료 코드 사실.
        exit: ExitFacts,
        /// 레벨별 항목 수(`ok`/`warn`/`fail`/해석 불가).
        counts: [usize; 4],
        /// 칸들.
        cells: RowCells,
    },
    /// 자식은 끝났지만 stdout을 status 문서로 읽지 못했다.
    ///
    /// ## 이 행의 레벨은 항상 [`Level::Error`]다 — 종료 코드 레벨이 아니다
    /// exit 0으로 끝났는데 stdout이 깨졌다면 "정상"이 아니라 **"모른다"**다. 종료 코드
    /// 레벨(`Ok`)을 그대로 쓰면 화면이 읽지도 못한 결과를 초록으로 칠한다. 그래서 행
    /// 레벨은 `Error`로 두고, 종료 코드의 의미는 [`ExitFacts`]와 설명 문장으로 **함께**
    /// 보여준다 — 그러면 exit 4·5도 "실패"라고 적히지 않는다(각자의 문구가 나온다).
    Unreadable {
        /// 종료 코드 사실.
        exit: ExitFacts,
        /// 종료 코드가 뜻하는 것(한 줄) — [`job_exit::present`]에서 온다.
        headline: String,
        /// 다음에 할 일 — [`job_exit::present`]에서 온다.
        detail: String,
        /// 왜 읽지 못했는지([`StatusError::explain`]).
        parse: String,
        /// 자식 stderr 발췌(마스킹 대상).
        stderr: String,
    },
    /// 자식을 돌리지 못했다(spawn 실패·타임아웃·예산 초과·이름 거부).
    Unavailable {
        /// 무슨 일이 있었는지 한 줄.
        headline: String,
        /// 무엇을 의심해야 하는지.
        detail: String,
    },
}

/// 프로파일 한 행.
#[derive(Debug, Clone)]
pub struct ProfileRow {
    /// 프로파일 이름(명부에서 온 것 — 자식이 보고한 이름과 다를 수 있으면 [`RowState`]가 말한다).
    pub profile: String,
    /// 엔진 라벨(명부의 `db`). URI를 해석하지 못한 프로파일은 `None`.
    pub engine: Option<String>,
    /// 이 행의 상태.
    pub state: RowState,
    /// 프로브 실측 소요 시간.
    pub elapsed: Duration,
    /// 이 값이 만들어진 뒤 흐른 시간(캐시 나이). 방금 실측했으면 0에 가깝다.
    pub age: Duration,
    /// 캐시에서 나왔는가.
    pub hit: bool,
}

impl ProfileRow {
    /// 행의 상태 레벨 — 표의 배지와 페이지 판정이 이 값을 쓴다.
    pub fn level(&self) -> Level {
        match &self.state {
            RowState::Reported { level, .. } => *level,
            // 결과를 얻지 못했다 — config가 나쁜 게 아니라 점검이 성립하지 않았다
            // ([`Level::Error`] doc의 `Fail`/`Error` 구분).
            RowState::Unreadable { .. } | RowState::Unavailable { .. } => Level::Error,
        }
    }
}

/// 명부의 항목 하나.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RosterEntry {
    /// 프로파일 이름(config의 `[profiles.<name>]`).
    pub profile: String,
    /// 엔진 라벨. `doctor`가 URI를 해석하지 못하면 `None`.
    pub engine: Option<String>,
}

/// 화면 전체가 그릴 상태.
#[derive(Debug)]
pub enum Dashboard {
    /// 명부를 얻었다. 행이 0개일 수도 있다(config에 프로파일이 없다).
    Rows {
        /// 프로파일별 행(명부 순서 = `doctor`가 이름순으로 정렬한 순서).
        rows: Vec<ProfileRow>,
        /// 명부 값의 나이.
        roster_age: Duration,
        /// 명부가 캐시에서 나왔는가.
        roster_hit: bool,
        /// 캐시 수명(화면이 "N초 캐시"라고 말하는 데 쓴다).
        ttl: Duration,
        /// 프로브 전체 실측 소요(명부 + 모든 배치).
        elapsed: Duration,
    },
    /// 명부 자체를 얻지 못했다 — 그릴 행이 없다.
    ///
    /// 행 하나의 실패와 다르게 취급하는 이유: 프로파일 목록을 모르면 부분 화면도 만들 수
    /// 없다. 이때 운영자가 봐야 할 곳은 대상 서버가 아니라 콘솔의 config 연결이다.
    NoRoster {
        /// 무슨 일이 있었는지 한 줄.
        headline: String,
        /// 무엇을 확인할지.
        detail: String,
        /// 자식 stderr 발췌(마스킹 대상).
        stderr: String,
    },
}

impl Dashboard {
    /// 페이지 배너 레벨 — 행 중 가장 나쁜 것.
    ///
    /// 행이 없으면 [`Level::Warn`]이다. config에 프로파일이 0개인 것은 오류가 아니지만
    /// (서버는 정상이다) 이 화면의 목적이 성립하지 않는 상태이므로 초록도 아니다.
    pub fn level(&self) -> Level {
        match self {
            Dashboard::Rows { rows, .. } => rows
                .iter()
                .map(ProfileRow::level)
                .max_by_key(|l| severity(*l))
                .unwrap_or(Level::Warn),
            Dashboard::NoRoster { .. } => Level::Error,
        }
    }

    /// 주의가 필요한 행 수(레벨이 `Ok`가 아닌 행).
    pub fn needs_attention(&self) -> usize {
        match self {
            Dashboard::Rows { rows, .. } => rows.iter().filter(|r| r.level() != Level::Ok).count(),
            Dashboard::NoRoster { .. } => 0,
        }
    }
}

// ---------------------------------------------------------------------------
// 텍스트 위생 — 발췌·URI 접기
// ---------------------------------------------------------------------------

/// 문자열을 [`EXCERPT_CHARS`]로 자른다. 문자 경계로 자르므로 멀티바이트가 깨지지 않는다.
///
/// `routes::doctor`에도 같은 함수가 있다(비공개, 상한만 다르다). 통합 후보다 —
/// [`severity`]와 같은 이유로 지금은 각자 들고 있다.
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(EXCERPT_CHARS).collect();
    format!("{head}…")
}

/// 텍스트 안의 `scheme://…` 토큰을 찾아 각각 [`crate::cli::output::redact_uri`]로 접는다.
///
/// ## 왜 [`SecretRegistry`]만으로는 부족한가
/// 레지스트리는 **알고 있는 값**만 지운다. 그 값들은 config가 `*_env`로 선언한 시크릿
/// 이고([`crate::web::job::JobSecrets`]), 기동 시점에 env에서 옮겨온 것들이다. 그런데
/// config에 접속 URI를 **인라인으로** 적는 것도 유효한 설정이다(`source.uri = "…"`).
/// 그렇게 적은 URI는 env를 거치지 않으므로 레지스트리에 없다.
///
/// 그리고 `status`의 연결 실패 항목 메시지는 드라이버 오류를 그대로 싣는다
/// (`"connection failed: {e}"` — `engine/mongo/status.rs`). 드라이버 오류 문자열에는
/// 접속 문자열이 들어올 수 있다. 즉 **레지스트리가 비어 있어도 비밀번호가 화면에
/// 도달할 수 있는 경로**가 이것 하나였다.
///
/// [`redact_uri`](crate::cli::output::redact_uri)는 URI **하나**를 받는 함수이므로
/// 산문에는 쓸 수 없다. 그래서 이 함수가 산문에서 URI 토큰만 골라내 그 함수에 넘긴다.
/// 호스트는 보존된다 — "어디로 연결하려 했는가"는 운영자가 봐야 하는 정보이고, 그걸
/// 통째로 `[REDACTED]`로 덮으면 진단이 불가능해진다(그쪽 함수 doc의 판단).
///
/// ## 왜 정규식이 아닌가
/// 이 크레이트에 `regex` 의존성이 없고, 이 목적에는 필요하지도 않다. 찾는 것은 리터럴
/// `"://"` 하나이고, 그 앞뒤로 허용 문자를 훑으면 끝난다.
///
/// ## 경계 규칙
/// - **스킴**: `://` 왼쪽으로 `[A-Za-z0-9+.-]`를 훑는다. 하나도 없으면 URI가 아니므로
///   건너뛴다(`" ://x"` 같은 문자열을 URI로 오인하지 않는다).
/// - **끝**: 공백과 산문에서 URI를 감싸는 문자(`"'`\`,;()[]{}<>`)에서 끊는다. 이 문자들은
///   URI에 나타날 수 있지만(경로·쿼리), 산문 안에서는 경계 신호일 가능성이 훨씬 크고,
///   과하게 끊는 실패는 "덜 지우는" 방향이 아니라 "덜 보여주는" 방향이다 — 안전한 쪽이다.
/// - 훑는 문자가 전부 ASCII이므로 바이트 인덱스가 항상 문자 경계에 떨어진다(멀티바이트
///   문자는 어느 집합에도 속하지 않아 즉시 경계가 된다).
pub fn redact_uris(text: &str) -> String {
    /// URI 스킴에 나타날 수 있는 문자.
    fn is_scheme_byte(b: u8) -> bool {
        b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.')
    }
    /// 산문 안에서 URI 토큰의 끝을 뜻하는 문자.
    fn is_boundary_byte(b: u8) -> bool {
        b.is_ascii_whitespace()
            || matches!(
                b,
                b'"' | b'\''
                    | b'`'
                    | b','
                    | b';'
                    | b'('
                    | b')'
                    | b'['
                    | b']'
                    | b'{'
                    | b'}'
                    | b'<'
                    | b'>'
            )
    }

    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    // 이미 출력에 복사한 위치.
    let mut copied = 0usize;
    // 다음 `://`를 찾을 시작 위치.
    let mut cursor = 0usize;

    while let Some(rel) = text[cursor..].find("://") {
        let sep = cursor + rel;
        // 스킴 시작 위치를 왼쪽으로 훑는다.
        let mut start = sep;
        while start > 0 && is_scheme_byte(bytes[start - 1]) {
            start -= 1;
        }
        if start == sep {
            // 스킴이 없다 — URI로 보지 않는다. `://` 뒤에서 다시 찾는다.
            cursor = sep + 3;
            continue;
        }
        // 토큰 끝을 오른쪽으로 훑는다.
        let mut end = sep + 3;
        while end < bytes.len() && !is_boundary_byte(bytes[end]) {
            end += 1;
        }
        // 스킴 시작이 이미 복사한 지점보다 앞일 수 없다(URI끼리 겹치지 않는다).
        let start = start.max(copied);
        out.push_str(&text[copied..start]);
        out.push_str(&crate::cli::output::redact_uri(&text[start..end]));
        copied = end;
        cursor = end;
    }
    out.push_str(&text[copied..]);
    out
}

/// C0 제어문자(줄바꿈·탭 제외)와 DEL을 U+FFFD로 바꾼다.
///
/// ## 왜 HTML 이스케이프만으로는 부족한가
/// maud는 `& < > " '`를 이스케이프하지만 **제어문자는 손대지 않는다** — HTML 문법상
/// 위험하지 않기 때문이다. 그런데 자식 stdout/stderr에는 널 바이트(비-UTF-8 바이트가
/// lossy 변환을 거친 흔적)나 ANSI 이스케이프 시퀀스가 섞일 수 있고, 그것이 화면에
/// 그대로 나가면 세 가지가 생긴다:
///
/// 1. **널 바이트는 HTML에서 유효하지 않다.** 브라우저·프록시·저장 도구가 각자 다르게
///    처리하고, 그 차이가 파서 혼란(mXSS)의 재료가 된다.
/// 2. **ESC 시퀀스는 이 HTML을 터미널로 옮기는 순간 살아난다.** 운영자가 화면을 복사해
///    붙이거나 로그로 저장하면 커서 이동·화면 지우기가 실행된다.
/// 3. **눈에 보이지 않는 문자가 진단을 왜곡한다.** 화면에 없는 것처럼 보이는 문자가
///    문자열 비교·검색을 어긋나게 한다.
///
/// `\n`과 `\t`는 남긴다 — `<pre class="logdump">`가 여러 줄 stderr를 보여주는 것이
/// 그 요소의 존재 이유이고, 둘은 HTML에서 안전하다.
fn strip_controls(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c == '\n' || c == '\t' {
                c
            } else if c.is_control() {
                char::REPLACEMENT_CHARACTER
            } else {
                c
            }
        })
        .collect()
}

/// 화면에 나가는 남의 텍스트를 세 겹으로 지운다 — **뷰가 부르는 유일한 위생 함수**.
///
/// 순서가 중요하다:
///
/// 1. **레지스트리 마스킹이 먼저다.** 정확히 일치하는 값을 통째로 `[REDACTED]`로
///    바꾸므로 가장 강한 방어다. URI 접기를 먼저 하면 URI 안에 있던 등록 값이
///    `***@host` 형태로 바뀌어 레지스트리의 정확 일치가 빗나간다.
/// 2. **URI 접기** — 레지스트리가 모르는 접속 문자열의 자격증명을 지운다([`redact_uris`]).
/// 3. **제어문자 제거** — 마지막이다([`strip_controls`]). 앞 두 단계가 원문을 봐야
///    하므로(제어문자를 먼저 바꾸면 등록 값과의 일치가 깨질 수 있다) 위생은 맨 뒤에 온다.
///
/// 세 단계 모두 멱등(idempotent)이다 — 이미 지워진 텍스트를 다시 통과시켜도 그대로다.
/// 그래서 값을 만드는 쪽이 이미 지웠더라도 뷰가 한 번 더 통과시킬 수 있다
/// (`routes::jobs` 헤더의 "마지막 방어선을 화면에 둔다").
pub fn scrub(registry: &SecretRegistry, text: &str) -> String {
    strip_controls(&redact_uris(&registry.mask(text)))
}

// ---------------------------------------------------------------------------
// 자식 실행 — 프로파일마다 하나, 요청 수명에 묶인 유한 실행
// ---------------------------------------------------------------------------

/// 명부를 얻는 잡 명세 — `doctor --json`.
///
/// `--config`/`--lang`은 붙이지 않는다. 러너가 기동 시점에 확정한 값으로 모든 잡에
/// 똑같이 붙이므로(`JobRunner::build_command`), 여기서 따로 붙이면 이 화면이 본 config와
/// 잡이 볼 config가 갈라질 수 있다 — `routes::doctor`가 방금 고친 버그의 본질이다.
fn roster_spec(lang: Lang) -> JobSpec {
    JobSpec::new(JobCommand::Doctor, lang)
}

/// 프로파일 하나의 상태를 얻는 잡 명세 — `status --profile <name> --json`.
///
/// `--all`을 쓰지 않는 이유는 모듈 헤더 참조. `--ns-detail`도 붙이지 않는다 — 그건
/// 컬렉션마다 카운트 질의를 날리는 무거운 옵션이고, 이 화면은 "지금 괜찮은가"만 묻는다.
fn status_spec(profile: &ProfileName, lang: Lang) -> JobSpec {
    JobSpec::new(JobCommand::Status, lang).with_profile(profile.clone())
}

/// 자식 하나를 상한까지만 돌려 관측 결과로 접는다.
///
/// 실패를 `Result`로 올리지 않고 [`ProbeOutput`]에 담아 돌려준다 — 이 값이 그대로
/// 캐시에 들어가기 때문이다([`crate::web::cache`] 헤더 "실패도 캐시한다").
async fn run_probe(runner: &JobRunner, spec: &JobSpec, limit: Duration) -> ProbeOutput {
    let started = Instant::now();
    let running = match runner.spawn(spec) {
        Ok(r) => r,
        Err(e) => {
            return ProbeOutput {
                outcome: ProbeOutcome::SpawnFailed(excerpt(&e.to_string())),
                elapsed: started.elapsed(),
            }
        }
    };
    await_with_limit(running, limit, started).await
}

/// [`run_probe`]의 결과를 캐시가 담는 형태([`Arc`])로 감싼다.
///
/// [`Arc`]인 이유: 캐시는 값을 소유하고 호출자에게 **사본**을 준다
/// ([`TtlCache`] doc). `ProbeOutput`은 자식 stdout 전체를 들고 있어 수 KB일 수 있고,
/// 프로파일 50개 화면이 그것을 요청마다 통째로 복제할 이유가 없다 — 참조 카운트
/// 하나로 끝낸다. `spec`을 값으로 받는 것은 이 퓨처가 캐시 안으로 들어가면서
/// 임시 명세보다 오래 살기 때문이다.
async fn shared_probe(runner: &JobRunner, spec: JobSpec, limit: Duration) -> Arc<ProbeOutput> {
    Arc::new(run_probe(runner, &spec, limit).await)
}

/// 돌고 있는 자식을 상한까지만 기다린다. 상한을 넘으면 `SIGKILL`을 보낸다.
///
/// ## 왜 `kill_on_drop`이 아니라 명시적 kill인가
/// 잡 러너는 `kill_on_drop`을 **의도적으로 켜지 않는다** — 2시간짜리 백업이 배포나
/// 핸들러의 조기 반환 때마다 죽으면 안 되기 때문이다([`crate::web::job::runner`] 헤더).
/// 그 정책을 이 화면 때문에 뒤집을 수 없으므로, "요청 하나 안에서 끝나야 하는 짧은 잡"에
/// 필요한 상한만 호출부에서 얹는다(`routes::doctor::await_with_limit`와 같은 구조).
///
/// 여기서 kill을 **반드시** 해야 하는 이유가 doctor보다 강하다: 이 화면은 한 요청에
/// 자식을 프로파일 수만큼 띄운다. 멈춘 자식을 남기면 새로고침마다 그만큼 쌓인다.
async fn await_with_limit(running: RunningJob, limit: Duration, started: Instant) -> ProbeOutput {
    let pid = running.pid();
    let outcome = match tokio::time::timeout(limit, running.wait_with_output()).await {
        Ok(Ok(done)) => ProbeOutcome::Completed {
            outcome: done.outcome,
            stdout: done.stdout,
            stderr: done.stderr,
        },
        Ok(Err(e)) => ProbeOutcome::WaitFailed(excerpt(&e.to_string())),
        Err(_) => {
            kill_child(pid);
            ProbeOutcome::TimedOut(limit)
        }
    };
    ProbeOutput {
        outcome,
        elapsed: started.elapsed(),
    }
}

/// 상한을 넘긴 자식에게 `SIGKILL`을 보낸다. pid를 모르면(이미 수거됨) 할 일이 없다.
///
/// 프로세스 **그룹**이 아니라 pid 하나에 보낸다. `status`는 읽기 전용 점검이라 외부
/// 도구를 띄우지 않으므로 정리할 손자가 없고, 그룹 시그널(`kill(-pid, …)`)은 환경에
/// 따라 `EPERM`으로 거부되는 사례가 이 프로젝트에서 이미 관측됐다 — 정리할 대상이
/// 없는데 실패할 수 있는 방법을 고를 이유가 없다.
///
/// `routes::doctor`·`config_write`에도 같은 함수가 있다. 공용 자리로 옮기는 것이 옳지만
/// 그 파일들은 이 태스크의 소유가 아니다 — **통합 후보로 남긴다**(보고에 적었다).
#[cfg(unix)]
fn kill_child(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    // pid를 `as`로 캐스팅하지 않는다 — 변환이 실패해 -1이 되면 `kill(-1, …)`은 "이
    // 사용자가 보낼 수 있는 모든 프로세스"를 뜻한다. 실패하면 아무것도 하지 않는 편이 맞다.
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: pid는 방금 우리가 spawn한, 아직 수거되지 않은 자식의 pid다(러너가 std에서
    // 받은 값). 값 자체를 신뢰할 수 없는 외부 입력이 아니므로 시그널 전송은 안전하다.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

/// 비-unix 플랫폼 스텁 — 이 프로젝트의 배포 대상은 darwin/linux뿐이다.
#[cfg(not(unix))]
fn kill_child(_pid: Option<u32>) {}

// ---------------------------------------------------------------------------
// 수집 — 캐시를 경유해 명부와 프로파일을 모은다
// ---------------------------------------------------------------------------

/// 이 화면이 쓰는 캐시의 타입 별칭 — 시그니처가 길어지는 것을 막는다.
pub type ProbeCache = TtlCache<ProbeKey, Arc<ProbeOutput>>;

/// 화면 상태를 모은다 — **이 모듈의 유일한 부작용 지점.**
///
/// 캐시를 인자로 받는 이유는 테스트다. 운영 경로는 전역 캐시를 쓰지만
/// ([`crate::web::cache::probe_cache`]), 캐시 동작을 검증하는 테스트는 자기 인스턴스를
/// 넘겨 다른 테스트와 격리된다.
pub async fn collect(ctx: &ServeConfig, cache: &ProbeCache, limits: ProbeLimits) -> Dashboard {
    let started = Instant::now();
    let config = cache::config_key(ctx);

    // 1) 명부 — 오프라인 `doctor --json` 한 번.
    let roster_probe = cache
        .fetch(
            ProbeKey::Roster {
                config: config.clone(),
            },
            || shared_probe(&ctx.jobs, roster_spec(ctx.lang), limits.roster),
        )
        .await;

    let roster = match read_roster(&roster_probe.value, ctx.lang) {
        Ok(entries) => entries,
        Err(dashboard) => return dashboard,
    };

    // 2) 프로파일 — 동시 폭을 제한한 배치로 나눠 돈다.
    //
    // 예산 시계는 **명부를 얻은 뒤에** 시작한다. 명부 시간까지 예산에 넣으면 느리지만
    // 성공한 명부(예: NFS에 있는 recipient_file 때문에 8초)가 프로파일 예산을 통째로
    // 태워, 화면이 "명부는 얻었는데 아무것도 점검하지 않았다"가 된다 — 최악의 결과다.
    // 그래서 두 상한은 더해진다(최악 = roster + budget). 그 합이 앞단 프록시 상한보다
    // 넉넉히 짧다는 것을 `default_limits_are_internally_consistent`가 고정한다.
    let deadline = Instant::now() + limits.budget;
    let width = limits.concurrency.max(1);
    let mut rows = Vec::with_capacity(roster.len());
    for chunk in roster.chunks(width) {
        let batch = chunk
            .iter()
            .map(|entry| probe_row(ctx, cache, &config, entry, limits, deadline));
        rows.extend(futures::future::join_all(batch).await);
    }

    Dashboard::Rows {
        rows,
        roster_age: roster_probe.age,
        roster_hit: roster_probe.hit,
        ttl: cache.ttl(),
        elapsed: started.elapsed(),
    }
}

/// 명부 프로브 결과를 프로파일 목록으로 읽는다. 못 읽으면 화면 전체가
/// [`Dashboard::NoRoster`]다.
///
/// `doctor`의 출력을 파싱하는 일은 [`doctor::parse_report`]가 이미 한다 — 이 화면이
/// 자기 파서를 만들지 않는다(스키마 검사·오류 어휘가 두 벌로 갈라질 이유가 없다).
fn read_roster(probe: &ProbeOutput, lang: Lang) -> Result<Vec<RosterEntry>, Dashboard> {
    let (outcome, stdout, stderr) = match &probe.outcome {
        ProbeOutcome::Completed {
            outcome,
            stdout,
            stderr,
        } => (outcome, stdout.as_str(), stderr.as_str()),
        ProbeOutcome::SpawnFailed(detail) => {
            return Err(Dashboard::NoRoster {
                headline: lang
                    .sel(
                        "The console could not start the profile inventory check.",
                        "콘솔이 프로파일 목록 점검을 시작할 수 없었습니다.",
                    )
                    .to_string(),
                detail: format!(
                    "{} {detail}",
                    lang.sel(
                        "The console runs checks with its own executable — verify that the binary is still readable and executable:",
                        "콘솔은 자기 실행 파일로 점검을 수행합니다 — 바이너리가 여전히 읽기·실행 가능한지 확인하세요:",
                    )
                ),
                stderr: String::new(),
            })
        }
        ProbeOutcome::WaitFailed(detail) => {
            return Err(Dashboard::NoRoster {
                headline: lang
                    .sel(
                        "Reading the profile inventory check's output failed.",
                        "프로파일 목록 점검의 출력을 읽는 데 실패했습니다.",
                    )
                    .to_string(),
                detail: detail.clone(),
                stderr: String::new(),
            })
        }
        ProbeOutcome::TimedOut(limit) => {
            return Err(Dashboard::NoRoster {
                headline: format!(
                    "{} ({}s)",
                    lang.sel(
                        "The profile inventory check did not finish within the time limit and was killed.",
                        "프로파일 목록 점검이 상한 시간 안에 끝나지 않아 종료시켰습니다.",
                    ),
                    limit.as_secs()
                ),
                detail: lang
                    .sel(
                        "doctor only reads config and stats files, so a stalled network mount under recipient_file is the usual cause.",
                        "doctor는 config와 파일 정보만 읽습니다 — recipient_file이 멈춘 네트워크 마운트에 있는 경우가 흔한 원인입니다.",
                    )
                    .to_string(),
                stderr: String::new(),
            })
        }
    };

    match doctor::parse_report(stdout.as_bytes()) {
        Ok(report) => Ok(report
            .profiles
            .into_iter()
            .map(|p| RosterEntry {
                profile: p.profile,
                engine: p.db,
            })
            .collect()),
        Err(error) => {
            let presented = job_exit::present(outcome, lang);
            Err(Dashboard::NoRoster {
                headline: error.explain(lang),
                // 종료 코드 설명을 함께 싣는다 — 파싱 실패의 원인은 대개 "자식이 JSON을
                // 찍기 전에 다른 이유로 끝났다"이고, 그 이유는 종료 코드에 남는다
                // (config 미연결이면 exit 2).
                detail: format!("{} {}", presented.headline, presented.detail),
                stderr: excerpt(stderr),
            })
        }
    }
}

/// 프로파일 하나의 행을 만든다.
///
/// ## 캐시 확인이 예산 확인보다 **먼저**인 이유
/// 캐시 히트는 자식을 띄우지 않으므로 예산을 쓰지 않는다. 순서를 뒤집으면 앞쪽
/// 프로파일들이 예산을 태운 뒤, 뒤쪽 프로파일은 **캐시에 살아 있는 값이 있는데도**
/// "미측정"으로 표시된다 — 화면이 스스로 아는 것을 모른다고 말하는 셈이다.
async fn probe_row(
    ctx: &ServeConfig,
    cache: &ProbeCache,
    config: &str,
    entry: &RosterEntry,
    limits: ProbeLimits,
    deadline: Instant,
) -> ProfileRow {
    // 명부의 이름을 그대로 `--profile` 값으로 쓰지 않는다 — argv 값 위치로 가는 모든
    // 문은 검증을 지난다([`crate::web::job::args`] 헤더). config에 적힌 이름이라도
    // 예외가 아니다: 대시보드가 config를 신뢰해 검증을 건너뛰면, config를 쓰는 다른
    // 경로(t28의 config 편집 화면)에서 들어온 이름이 여기로 흘러들 수 있다.
    let name = match ProfileName::parse(&entry.profile, ctx.lang) {
        Ok(name) => name,
        Err(e) => {
            return ProfileRow {
                profile: entry.profile.clone(),
                engine: entry.engine.clone(),
                state: RowState::Unavailable {
                    headline: ctx
                        .lang
                        .sel(
                            "This profile name cannot be passed to a command.",
                            "이 프로파일 이름은 명령 인자로 넘길 수 없습니다.",
                        )
                        .to_string(),
                    detail: format!(
                        "{} {}",
                        ctx.lang.sel(
                            "Rename the profile in config; the console refuses to build an argument from it:",
                            "config에서 프로파일 이름을 바꾸세요. 콘솔은 이 이름으로 인자를 만들지 않습니다:",
                        ),
                        excerpt(&e.to_string())
                    ),
                },
                elapsed: Duration::ZERO,
                age: Duration::ZERO,
                hit: false,
            }
        }
    };

    let key = ProbeKey::Profile {
        config: config.to_string(),
        profile: name.as_str().to_string(),
    };

    // 1) 살아 있는 캐시본이 있으면 예산과 무관하게 쓴다(위 doc 참조).
    if let Some(cached) = cache.peek(&key) {
        return row_from_probe(entry, &cached.value, cached.age, cached.hit, ctx.lang);
    }
    // 2) 예산이 남았는지 본다. 없으면 이 프로파일은 이번 요청에서 보지 않는다.
    if Instant::now() >= deadline {
        return ProfileRow {
            profile: entry.profile.clone(),
            engine: entry.engine.clone(),
            state: RowState::Unavailable {
                headline: ctx
                    .lang
                    .sel(
                        "Not checked in this request — the page ran out of its probe budget.",
                        "이번 요청에서 점검하지 않았습니다 — 화면의 프로브 예산이 끝났습니다.",
                    )
                    .to_string(),
                detail: format!(
                    "{} ({}s)",
                    ctx.lang.sel(
                        "This is not a failure and says nothing about the profile. Earlier profiles were slow; reload to check this one, or reduce how many profiles one console serves.",
                        "이것은 실패가 아니며 이 프로파일에 대해 아무것도 말해 주지 않습니다. 앞선 프로파일들이 느렸다는 뜻입니다 — 새로고침하면 이 프로파일을 점검합니다.",
                    ),
                    limits.budget.as_secs()
                ),
            },
            elapsed: Duration::ZERO,
            age: Duration::ZERO,
            hit: false,
        };
    }
    // 3) 실제 프로브.
    let probe = cache
        .fetch(key, || {
            shared_probe(&ctx.jobs, status_spec(&name, ctx.lang), limits.profile)
        })
        .await;
    row_from_probe(entry, &probe.value, probe.age, probe.hit, ctx.lang)
}

/// 프로브 결과 하나를 행으로 접는다 — **순수 함수**(자식도 캐시도 건드리지 않는다).
///
/// 이 함수가 순수해야 하는 이유는 [`crate::web::routes`] 모듈 헤더의 규약과 같다:
/// "exit 4를 경고로 그리는가"·"stdout이 깨졌을 때 무엇을 그리는가" 같은 판정을 자식을
/// 실제로 띄우지 않고 단위 테스트로 고정할 수 있어야 한다.
pub fn row_from_probe(
    entry: &RosterEntry,
    probe: &ProbeOutput,
    age: Duration,
    hit: bool,
    lang: Lang,
) -> ProfileRow {
    let state = match &probe.outcome {
        ProbeOutcome::Completed {
            outcome,
            stdout,
            stderr,
        } => row_state_from_output(outcome, stdout, stderr, lang),
        ProbeOutcome::SpawnFailed(detail) => RowState::Unavailable {
            headline: lang
                .sel(
                    "The console could not start the check for this profile.",
                    "콘솔이 이 프로파일의 점검을 시작할 수 없었습니다.",
                )
                .to_string(),
            detail: format!(
                "{} {detail}",
                lang.sel(
                    "Verify that the console's own binary is still readable and executable:",
                    "콘솔 자신의 바이너리가 여전히 읽기·실행 가능한지 확인하세요:",
                )
            ),
        },
        ProbeOutcome::WaitFailed(detail) => RowState::Unavailable {
            headline: lang
                .sel(
                    "Reading this profile's check output failed.",
                    "이 프로파일의 점검 출력을 읽는 데 실패했습니다.",
                )
                .to_string(),
            detail: detail.clone(),
        },
        ProbeOutcome::TimedOut(limit) => RowState::Unavailable {
            headline: format!(
                "{} ({}s)",
                lang.sel(
                    "The check did not finish within the per-profile time limit and was killed.",
                    "점검이 프로파일별 상한 시간 안에 끝나지 않아 종료시켰습니다.",
                ),
                limit.as_secs()
            ),
            detail: lang
                .sel(
                    "The other profiles on this page were unaffected. Suspect the target server, the network, or a stalled destination mount — not the console.",
                    "이 화면의 다른 프로파일은 영향을 받지 않았습니다. 콘솔이 아니라 대상 서버·네트워크·멈춘 destination 마운트를 의심하세요.",
                )
                .to_string(),
        },
    };
    ProfileRow {
        profile: entry.profile.clone(),
        engine: entry.engine.clone(),
        state,
        elapsed: probe.elapsed,
        age,
        hit,
    }
}

/// 끝난 자식의 stdout/stderr를 행 상태로 접는다 — **순수 함수**.
///
/// ## 행 레벨은 항목 최악과 종료 코드 레벨 중 더 나쁜 쪽이다
/// 정상 CLI에서 둘은 어긋날 수 없다 — `status`의 종료 코드는 `overall`(항목 합산)에서
/// 계산되기 때문이다(`status.rs::report_to_result`). 그런데도 둘 중 나쁜 쪽을 쓰는 이유:
///
/// 1. **어긋남 자체가 신호다.** 콘솔과 CLI가 다른 빌드이거나 출력이 부분적으로 손상된
///    경우 둘이 갈라질 수 있고, 그때 항목만 보고 초록으로 칠하면 화면이 거짓말을 한다.
/// 2. **exit 2를 놓치지 않는다.** 사용법 오류로 끝났는데도 어쩌다 JSON이 남아 있으면,
///    그 보고서는 우리가 물어본 것과 다른 대상에 관한 것일 수 있다. `Rejected`의 레벨
///    ([`Level::Error`])이 그 의심을 화면에 남긴다.
///
/// **exit 4·5가 이 규칙으로 실패가 되는 일은 없다** — 둘의 레벨은 [`Level::Warn`]이고,
/// 항목이 `fail`을 담고 있다면 애초에 exit이 3이었을 것이다.
fn row_state_from_output(outcome: &JobOutcome, stdout: &str, stderr: &str, lang: Lang) -> RowState {
    let exit = ExitFacts::of(outcome);
    match parse_status(stdout) {
        Ok(doc) => RowState::Reported {
            level: [worst_level(&doc.items), exit.level]
                .into_iter()
                .max_by_key(|l| severity(*l))
                .unwrap_or(Level::Error),
            overall: doc.overall.clone(),
            exit,
            counts: count_by_level(&doc.items),
            cells: cells_of(&doc),
        },
        Err(error) => {
            let presented = job_exit::present(outcome, lang);
            RowState::Unreadable {
                exit,
                headline: presented.headline,
                detail: presented.detail,
                parse: error.explain(lang),
                stderr: excerpt(stderr),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// 이 요청에서 쓸 시크릿 레지스트리.
///
/// **잡 러너의 레지스트리를 그대로 복제해 쓴다** — 자식에게 실제로 주입되는 값들이 곧
/// 자식 출력에 섞일 수 있는 값들이고, 형제 라우트(`routes::jobs`·`routes::backup`·
/// `routes::doctor`)도 같은 것을 쓴다. env에서 값을 다시 읽으면 기동 시점에 이미 제거된
/// 시크릿을 못 찾아 등록이 0건이 된다(그 회귀는 `routes::doctor` 헤더에 실측이 남아 있다).
///
/// 여기에 콘솔 세션 토큰을 얹는다. 그 값은 잡 자식에게 주입되지 않지만(러너 화이트리스트에
/// 없다), 우리 프로세스가 그리는 텍스트에 우연히 섞이는 경로를 막을 값어치가 있다.
/// [`AuthState`]에서 직접 받으므로 env든 파일이든 출처와 무관하게 등록된다.
fn request_registry(ctx: &ServeConfig, auth: Option<&AuthState>) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    if let Some(auth) = auth {
        auth.register_session_secret(&mut registry);
    }
    registry
}

/// `GET /dashboard` 핸들러.
///
/// 전역 캐시([`crate::web::cache::probe_cache`])와 운영 기본 상한
/// ([`ProbeLimits::default`])을 쓴다. 그 둘을 인자로 받는 [`collect`]가 따로 있는 이유는
/// 테스트 격리다(그 함수 doc 참조).
pub async fn page(
    State(ctx): State<Arc<ServeConfig>>,
    Extension(auth): Extension<Arc<AuthState>>,
) -> Markup {
    let dashboard = collect(&ctx, cache::probe_cache(), ProbeLimits::default()).await;
    let body = view::body(
        ctx.lang,
        ctx.config_path.is_some(),
        &dashboard,
        &request_registry(&ctx, Some(&auth)),
    );
    layout::shell(ctx.lang, DASHBOARD_TITLE, body)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::{self, AuthState};
    use crate::web::cache::PROBE_TTL;
    use crate::web::job::{JobRunner, JobSecrets};
    use axum::body::Body;
    use axum::http::{header, Request, StatusCode};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU32, Ordering};
    use tower::ServiceExt;

    /// 테스트 세션 토큰.
    const TEST_TOKEN: &str = "test-token-dashboard-5d81";

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

    /// 테스트가 쓰는 짧은 상한 — **값이 아니라 메커니즘**을 검증한다(본문 doc 참조).
    fn fast_limits() -> ProbeLimits {
        ProbeLimits {
            roster: Duration::from_secs(5),
            profile: Duration::from_millis(400),
            budget: Duration::from_secs(5),
            concurrency: 4,
        }
    }

    /// 실제 `doctor --json` 출력 모양의 명부(프로파일 3개 · 엔진 3종).
    const ROSTER_JSON: &str = r#"{"schema":1,"overall":"ok","profiles":[
        {"profile":"mongo-analytics","db":"mongodb","items":[{"label":"source","status":"ok","message":"resolved → mongodb"}]},
        {"profile":"mysql-billing","db":"mysql","items":[{"label":"source","status":"ok","message":"resolved → mysql"}]},
        {"profile":"pg-staging","db":"postgresql","items":[{"label":"source","status":"ok","message":"resolved → postgresql"}]}]}"#;

    /// 실제 `status --json` 출력 모양 — **여러 줄 pretty에 `schema`가 마지막 키**다.
    ///
    /// 이 모양이 모듈 헤더가 경고한 함정 그 자체다. 손으로 compact JSON을 적어 두면
    /// 파서가 pretty를 못 읽는 회귀를 놓친다.
    fn status_json(profile: &str, overall: &str, connection: &str, free: &str) -> String {
        let doc = serde_json::json!({
            "profile": profile,
            "items": [
                {"key":"connection","label":"connection","status":connection,
                 "message":"connected (user=backup, mechanism=SCRAM-SHA-256)","value":"SCRAM-SHA-256"},
                {"key":"topology","label":"topology","status":"ok",
                 "message":"replica set rs0 (3 members)","value":"replica set"},
                {"key":"replication_lag","label":"replication lag","status":"ok",
                 "message":"lag 0.4s behind primary","value":"0.4s"},
                {"key":"last_backup","label":"last backup","status":"ok",
                 "message":"full, 1.2 GiB, 3h ago (id 0190f0a2)","value":"3h ago"},
                {"key":"destination","label":"destination","status":"ok",
                 "message":format!("writable (local: /srv/backups/{profile}), free {free}"),"value":"OK"}
            ],
            "overall": overall,
            "schema": STATUS_JSON_SCHEMA,
        });
        // 단일 프로파일 `status --json`은 pretty다(`render_json` → `to_string_pretty`).
        serde_json::to_string_pretty(&doc).expect("표본 직렬화 실패")
    }

    // -- 가짜 실행 파일 ---------------------------------------------------
    //
    // 잡 러너의 `with_exe`에 셸 스크립트를 물려, **운영 경로 그대로**(`spawn` →
    // `wait_with_output`) 자식을 띄운다. 스크립트가 호출마다 카운터 파일에 한 줄을
    // 붙이므로 "자식이 몇 번 떴는가"를 프로세스 수준에서 셀 수 있다 — 캐시 히트에
    // spawn이 없다는 단정을 클로저 카운터가 아니라 실제 프로세스로 확인한다.

    /// 테스트마다 다른 임시 디렉터리(pid + 순번). 전역 캐시와 config 키가 섞이지 않게 한다.
    fn scratch(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "xb-dash-{tag}-{}-{}",
            std::process::id(),
            SEQ.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).expect("스크래치 디렉터리 생성 실패");
        dir
    }

    /// 가짜 `x-backup` 실행 파일을 만든다.
    ///
    /// - `doctor`가 오면 [`ROSTER_JSON`]을 찍는다.
    /// - `status --profile <name>`이 오면 `<dir>/status-<name>.json`을 찍고
    ///   `<dir>/exit-<name>`이 있으면 그 코드로 끝난다.
    /// - `<dir>/slow-<name>`이 있으면 먼저 5초 잔다(타임아웃 관측용).
    /// - 호출마다 `<dir>/spawns`에 한 줄을 붙인다.
    #[cfg(unix)]
    fn fake_exe(dir: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-x-backup");
        let script = r#"#!/bin/sh
dir="$(dirname "$0")"
profile=""
verb=""
prev=""
for a in "$@"; do
  case "$prev" in --profile) profile="$a" ;; esac
  case "$a" in doctor|status) [ -z "$verb" ] && verb="$a" ;; esac
  prev="$a"
done
echo "$verb $profile" >> "$dir/spawns"
if [ "$verb" = "doctor" ]; then
  cat "$dir/roster.json"
  exit 0
fi
[ -f "$dir/slow-$profile" ] && sleep 5
if [ -f "$dir/status-$profile.json" ]; then
  cat "$dir/status-$profile.json"
else
  printf 'boom: no such profile %s\n' "$profile" >&2
fi
if [ -f "$dir/exit-$profile" ]; then
  exit "$(cat "$dir/exit-$profile")"
fi
exit 0
"#;
        std::fs::write(&path, script).expect("가짜 실행 파일 쓰기 실패");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("실행 권한 부여 실패");
        std::fs::write(dir.join("roster.json"), ROSTER_JSON).expect("명부 표본 쓰기 실패");
        path
    }

    /// 가짜 실행 파일을 물린 테스트 컨텍스트 + 그 스크래치 디렉터리.
    ///
    /// `config_path`를 실제 파일로 채운다 — 캐시 키가 config 경로를 담으므로
    /// ([`cache::config_key`]) 테스트마다 다른 경로여야 전역 캐시를 공유해도 섞이지 않는다.
    #[cfg(unix)]
    fn ctx_with_fake_exe(tag: &str) -> (Arc<ServeConfig>, PathBuf) {
        let dir = scratch(tag);
        let exe = fake_exe(&dir);
        let config = dir.join("x-backup.toml");
        std::fs::write(
            &config,
            "# 이 파일은 가짜 자식이 읽지 않는다(경로만 쓴다)\n",
        )
        .expect("config 표본 쓰기 실패");
        let mut ctx = ServeConfig::for_test();
        ctx.config_path = Some(config);
        ctx.jobs = Arc::new(JobRunner::with_exe(
            exe,
            ctx.config_path.clone(),
            Lang::En,
            JobSecrets::new(),
        ));
        (Arc::new(ctx), dir)
    }

    /// 프로파일 하나의 status 표본을 심는다.
    fn plant_status(dir: &Path, profile: &str, json: &str) {
        std::fs::write(dir.join(format!("status-{profile}.json")), json)
            .expect("status 표본 쓰기 실패");
    }

    /// 세 프로파일 전부에 건강한 표본을 심는다.
    fn plant_healthy(dir: &Path) {
        for (profile, free) in [
            ("mongo-analytics", "12.3 GiB"),
            ("mysql-billing", "480.0 GiB"),
            ("pg-staging", "1.9 TiB"),
        ] {
            plant_status(dir, profile, &status_json(profile, "ok", "ok", free));
        }
    }

    /// 지금까지 뜬 자식 수(가짜 실행 파일이 남긴 줄 수).
    fn spawn_count(dir: &Path) -> usize {
        std::fs::read_to_string(dir.join("spawns"))
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    /// 테스트 전용 캐시 — 전역과 격리된다.
    fn test_cache() -> ProbeCache {
        TtlCache::new(Duration::from_secs(600))
    }

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    /// 이 화면 하나만 담은 최소 라우터 — **리더가 배선할 모양 그대로** 조립한다.
    ///
    /// 순서가 `server.rs::router`와 같다: 인증은 `route_layer`(매칭된 경로에만),
    /// `Extension<Arc<AuthState>>`는 그 밖의 `layer`(핸들러 값 주입). 둘을 뒤집으면
    /// Extension이 인증 미들웨어보다 안쪽에 들어가 핸들러가 그것을 못 뽑고 500이 된다 —
    /// 실제로 그렇게 만들었다가 이 테스트가 잡았다.
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(DASHBOARD_PATH, get(page))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            // `for_test`는 이미 `Arc<AuthState>`를 돌려준다 — 여기서 `Arc::new`를 한 번 더
            // 감싸면 `Arc<Arc<AuthState>>`가 들어가고, 핸들러가 요구하는
            // `Extension<Arc<AuthState>>`는 없는 것이 되어 500이 난다.
            .layer(Extension(Arc::clone(&auth_and_session().0)))
            .with_state(ctx)
    }

    /// 요청 하나를 보낸다. `cookie`가 `Some`이면 세션 쿠키를 싣는다.
    async fn call(ctx: Arc<ServeConfig>, cookie: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(DASHBOARD_PATH);
        if let Some(token) = cookie {
            builder = builder.header(
                header::COOKIE,
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

    // -- 파싱 -------------------------------------------------------------

    /// **여러 줄 pretty에 `schema`가 마지막 키인** 실제 모양이 온전히 파싱된다 —
    /// 모듈 헤더가 경고한 함정의 회귀 고정.
    #[test]
    fn multiline_pretty_status_output_parses() {
        let raw = status_json("prod", "warn", "ok", "9.0 GiB");
        assert!(raw.lines().count() > 5, "표본이 pretty가 아니다");
        assert!(
            raw.trim_end().ends_with('}') && raw.contains("\"schema\""),
            "표본 모양이 실제 출력과 다르다"
        );
        // 어느 한 줄도 완전한 JSON이 아니다 — 스트리밍 판별기가 실패하는 이유.
        for line in raw.lines() {
            assert!(
                serde_json::from_str::<serde_json::Value>(line).is_err(),
                "한 줄이 완전한 JSON이다 — 표본이 함정을 재현하지 못한다: {line}"
            );
        }
        let doc = parse_status(&raw).expect("pretty 출력이 파싱되지 않는다");
        assert_eq!(doc.schema, STATUS_JSON_SCHEMA);
        assert_eq!(doc.profile, "prod");
        assert_eq!(doc.overall, "warn");
        assert_eq!(doc.items.len(), 5);
        assert_eq!(doc.items[0].key, "connection");
        assert_eq!(doc.items[3].value.as_deref(), Some("3h ago"));
    }

    /// 병리적으로 깊은 stdout은 `serde_json`에 **닿기 전에** 거부되고, 그 결과는 이
    /// 프로파일 행 하나의 진단으로만 남는다.
    #[test]
    fn pathologically_deep_stdout_is_refused_before_parsing() {
        let bomb = "[".repeat(50_000);
        assert_eq!(
            parse_status(&bomb).unwrap_err(),
            StatusError::TooDeep {
                found: 50_000,
                max: jsonguard::MAX_JSON_DEPTH,
            }
        );
        // 상한 안쪽은 깊이를 이유로 거부되지 않는다.
        let nested = format!(
            r#"{{"schema":{},"items":[{{"deep":{}{}}}]}}"#,
            STATUS_JSON_SCHEMA,
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(
            !matches!(parse_status(&nested), Err(StatusError::TooDeep { .. })),
            "상한 안쪽 문서가 깊이를 이유로 거부됐다"
        );
    }

    /// 손상·빈·비-UTF8 대체문자·스키마 불일치 입력이 전부 패닉 없이 오류로 접히고,
    /// 각각 다른 진단 문장을 낸다.
    #[test]
    fn broken_status_output_becomes_readable_errors() {
        assert_eq!(parse_status("").unwrap_err(), StatusError::Empty);
        assert_eq!(parse_status("  \n\t ").unwrap_err(), StatusError::Empty);
        assert!(matches!(
            parse_status("{\"schema\":1,").unwrap_err(),
            StatusError::Malformed(_)
        ));
        assert!(matches!(
            parse_status("not json at all").unwrap_err(),
            StatusError::Malformed(_)
        ));
        assert!(matches!(
            parse_status("{}").unwrap_err(),
            StatusError::Malformed(_)
        ));
        // 러너가 lossy 변환한 비-UTF8 바이트(U+FFFD)는 JSON이 아니다.
        assert!(matches!(
            parse_status(&String::from_utf8_lossy(&[b'{', 0xFF, b'}'])).unwrap_err(),
            StatusError::Malformed(_)
        ));
        assert_eq!(
            parse_status(r#"{"schema":99,"profile":"p","overall":"ok","items":[]}"#).unwrap_err(),
            StatusError::SchemaMismatch {
                found: 99,
                expected: STATUS_JSON_SCHEMA
            }
        );
        // 스키마는 맞지만 items 모양이 다르다.
        assert!(matches!(
            parse_status(r#"{"schema":1,"profile":"p","overall":"ok","items":"nope"}"#)
                .unwrap_err(),
            StatusError::Malformed(_)
        ));

        // 세 종류의 설명이 서로 다르다 — 뭉개지면 운영자가 의심할 곳을 못 찾는다.
        let explains: Vec<String> = [
            StatusError::Empty,
            StatusError::Malformed("boom".into()),
            StatusError::SchemaMismatch {
                found: 2,
                expected: 1,
            },
        ]
        .iter()
        .map(|e| e.explain(Lang::En))
        .collect();
        for (i, a) in explains.iter().enumerate() {
            for b in &explains[i + 1..] {
                assert_ne!(a, b, "진단 문장이 겹친다");
            }
        }
    }

    // -- 판정 -------------------------------------------------------------

    /// **exit 4·5가 어느 경로에서도 실패로 접히지 않는다** — 이 화면의 핵심 불변식.
    ///
    /// 레벨 매핑을 이 파일이 자기 `match`로 다시 만들지 않으므로([`ExitFacts::of`]가
    /// [`job_exit::level_for_label`]을 부른다) 이 단정은 사실 그 함수의 성질을 이
    /// 화면에서 확인하는 것이다 — 나중에 누가 여기에 사본을 만들면 이 테스트가 깨진다.
    #[test]
    fn exit_four_and_five_are_never_failures() {
        let cases = [
            (JobOutcome::Succeeded, Level::Ok),
            (JobOutcome::PrecheckFailed, Level::Fail),
            (JobOutcome::SucceededWithWarnings, Level::Warn),
            (JobOutcome::LockConflict, Level::Warn),
        ];
        for (outcome, expected) in cases {
            let facts = ExitFacts::of(&outcome);
            assert_eq!(facts.level, expected, "{outcome:?} 레벨이 다르다");
        }
        for outcome in [JobOutcome::SucceededWithWarnings, JobOutcome::LockConflict] {
            assert_ne!(
                ExitFacts::of(&outcome).level,
                Level::Fail,
                "{outcome:?}가 실패로 접혔다"
            );
        }
        // exit 4와 5는 레벨이 같다(둘 다 "실패가 아니다") — 그래서 **문구가 겹치면
        // 안 된다**. 배지 색이 같아도 글자를 읽으면 다른 이야기여야 한다.
        for lang in [Lang::En, Lang::Ko] {
            let four = job_exit::present(&JobOutcome::SucceededWithWarnings, lang);
            let five = job_exit::present(&JobOutcome::LockConflict, lang);
            assert_ne!(
                four.headline, five.headline,
                "4와 5의 헤드라인이 같다({lang:?})"
            );
            assert_ne!(four.detail, five.detail, "4와 5의 디테일이 같다({lang:?})");
        }
        // 표기에 코드와 라벨이 함께 들어간다.
        assert_eq!(
            ExitFacts::of(&JobOutcome::SucceededWithWarnings).text(),
            "exit 4 · succeeded-with-warnings"
        );
        // 시그널 종료는 코드가 없으므로 라벨만 남는다(빈 `exit ` 표기를 만들지 않는다).
        assert_eq!(ExitFacts::of(&JobOutcome::Signaled(9)).text(), "signaled");
    }

    /// exit 0/3/4/5가 화면 표현에서 서로 구분된다(레벨 또는 문구 중 하나로라도).
    #[test]
    fn four_exit_codes_are_distinguishable_on_screen() {
        let outcomes = [
            JobOutcome::Succeeded,
            JobOutcome::PrecheckFailed,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::LockConflict,
        ];
        for (i, a) in outcomes.iter().enumerate() {
            for b in &outcomes[i + 1..] {
                let fa = ExitFacts::of(a);
                let fb = ExitFacts::of(b);
                assert!(
                    fa.level != fb.level || fa.text() != fb.text(),
                    "{a:?}와 {b:?}가 화면에서 완전히 같게 보인다"
                );
            }
        }
    }

    /// **읽을 수 없는 stdout은 종료 코드가 0이어도 초록이 아니다** — 읽지 못한 결과를
    /// 정상으로 칠하는 것이 이 화면이 할 수 있는 가장 나쁜 거짓말이다.
    #[test]
    fn unreadable_output_is_never_green_even_on_exit_zero() {
        let entry = RosterEntry {
            profile: "prod".into(),
            engine: Some("mongodb".into()),
        };
        for outcome in [
            JobOutcome::Succeeded,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::LockConflict,
            JobOutcome::PrecheckFailed,
        ] {
            let probe = ProbeOutput {
                outcome: ProbeOutcome::Completed {
                    outcome,
                    stdout: "garbage not json".into(),
                    stderr: "child said something".into(),
                },
                elapsed: Duration::from_millis(12),
            };
            let row = row_from_probe(&entry, &probe, Duration::ZERO, false, Lang::En);
            assert_eq!(
                row.level(),
                Level::Error,
                "{outcome:?}: 읽지 못한 결과가 Error가 아니다"
            );
            let RowState::Unreadable { exit, headline, .. } = &row.state else {
                panic!("{outcome:?}: Unreadable이 아니다: {:?}", row.state);
            };
            // 종료 코드의 의미는 잃지 않는다 — 사실과 설명이 함께 남는다.
            assert_eq!(exit.label, outcome.label());
            assert!(!headline.is_empty());
        }
    }

    /// **읽을 수 있는 출력에서도 exit 0/3/4/5가 서로 다른 레벨로 그려지고, 4·5는
    /// 실패가 아니다** — 정상 경로의 회귀 고정.
    ///
    /// 항목을 전부 `ok`로 두고 종료 코드만 바꾼다. 그러면 행 레벨을 정하는 것이 종료
    /// 코드뿐이어서, [`row_state_from_output`]의 "둘 중 나쁜 쪽" 규칙이 실제로 종료 코드를
    /// 존중하는지가 드러난다(항목만 보면 넷 다 초록이 됐을 것이다).
    #[test]
    fn readable_rows_respect_exit_codes_without_calling_four_or_five_a_failure() {
        let entry = RosterEntry {
            profile: "prod".into(),
            engine: Some("mongodb".into()),
        };
        let healthy = status_json("prod", "ok", "ok", "9.0 GiB");
        let expected = [
            (JobOutcome::Succeeded, Level::Ok),
            (JobOutcome::PrecheckFailed, Level::Fail),
            (JobOutcome::SucceededWithWarnings, Level::Warn),
            (JobOutcome::LockConflict, Level::Warn),
        ];
        for (outcome, want) in expected {
            let probe = ProbeOutput {
                outcome: ProbeOutcome::Completed {
                    outcome,
                    stdout: healthy.clone(),
                    stderr: String::new(),
                },
                elapsed: Duration::from_millis(30),
            };
            let row = row_from_probe(&entry, &probe, Duration::ZERO, false, Lang::En);
            assert_eq!(row.level(), want, "{outcome:?} 행 레벨이 다르다");
            assert!(
                matches!(&row.state, RowState::Reported { .. }),
                "{outcome:?}: 읽을 수 있는 출력이 진단으로 접혔다"
            );
            // 렌더에도 그 레벨과 종료 코드 표기가 함께 나간다.
            let dashboard = Dashboard::Rows {
                rows: vec![row],
                roster_age: Duration::ZERO,
                roster_hit: false,
                ttl: PROBE_TTL,
                elapsed: Duration::from_millis(30),
            };
            let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
            assert!(
                html.contains(&format!(r#"data-level="{}""#, want.token())),
                "{outcome:?}: 레벨 토큰이 마크업에 없다"
            );
            assert!(
                html.contains(outcome.label()),
                "{outcome:?}: 종료 코드 라벨이 화면에 없다"
            );
        }
        // 4·5는 어느 쪽도 FAIL 배지를 내지 않는다.
        for outcome in [JobOutcome::SucceededWithWarnings, JobOutcome::LockConflict] {
            let probe = ProbeOutput {
                outcome: ProbeOutcome::Completed {
                    outcome,
                    stdout: healthy.clone(),
                    stderr: String::new(),
                },
                elapsed: Duration::ZERO,
            };
            let row = row_from_probe(&entry, &probe, Duration::ZERO, false, Lang::En);
            let dashboard = Dashboard::Rows {
                rows: vec![row],
                roster_age: Duration::ZERO,
                roster_hit: false,
                ttl: PROBE_TTL,
                elapsed: Duration::ZERO,
            };
            let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
            assert!(
                !html.contains(r#"data-level="fail""#),
                "{outcome:?}가 화면에서 실패로 그려졌다"
            );
        }
    }

    /// 칸 추출 — 값이 있으면 값을, 없으면 메시지를 쓰고, 없는 키는 빈 칸이 된다.
    #[test]
    fn cells_prefer_values_and_tolerate_missing_keys() {
        let doc = parse_status(&status_json("prod", "ok", "ok", "9.0 GiB")).unwrap();
        let cells = cells_of(&doc);
        assert_eq!(cells.connection.as_ref().unwrap().text, "SCRAM-SHA-256");
        assert_eq!(cells.last_backup.as_ref().unwrap().text, "3h ago");
        assert_eq!(cells.destination.as_ref().unwrap().text, "OK");
        // destination의 여유 공간은 메시지에 있다(본문 doc 참조).
        assert!(
            cells
                .destination
                .as_ref()
                .unwrap()
                .note
                .contains("free 9.0 GiB"),
            "여유 공간이 근거 문장에 없다"
        );

        // `value`가 없는 항목은 메시지가 칸 값이 된다.
        let no_value = r#"{"schema":1,"profile":"p","overall":"ok","items":[
            {"key":"connection","label":"connection","status":"ok","message":"connected"}]}"#;
        let doc = parse_status(no_value).unwrap();
        let cells = cells_of(&doc);
        assert_eq!(cells.connection.as_ref().unwrap().text, "connected");
        // PostgreSQL에는 `secondary`가 없고 MongoDB에는 `replication_lag`가 없다 —
        // 없는 키는 오류가 아니라 빈 칸이다.
        assert!(cells.topology.is_none(), "없는 키가 칸을 만들었다");
        assert!(cells.replication.is_none());
        assert!(cells.destination.is_none());
    }

    /// 엔진마다 다른 키가 같은 `Replication` 칸으로 접힌다.
    #[test]
    fn replication_cell_accepts_engine_specific_keys() {
        for (key, value) in [
            ("replication_lag", "0.4s"),
            ("secondary", "members 3"),
            ("replication", "streaming"),
        ] {
            let raw = format!(
                r#"{{"schema":1,"profile":"p","overall":"ok","items":[
                    {{"key":"{key}","label":"l","status":"ok","message":"m","value":"{value}"}}]}}"#
            );
            let doc = parse_status(&raw).unwrap();
            let cell = cells_of(&doc)
                .replication
                .unwrap_or_else(|| panic!("'{key}'가 칸으로 접히지 않았다"));
            assert_eq!(cell.text, value);
        }
    }

    /// 항목 없는 문서는 초록이 아니다 — 점검이 성립하지 않은 것이다.
    #[test]
    fn empty_item_list_is_not_ok() {
        let doc = parse_status(r#"{"schema":1,"profile":"p","overall":"ok","items":[]}"#).unwrap();
        assert_eq!(worst_level(&doc.items), Level::Error);
        assert_eq!(count_by_level(&doc.items), [0, 0, 0, 0]);
    }

    // -- URI 접기 ---------------------------------------------------------

    /// **레지스트리가 모르는 URI의 비밀번호가 산문에서 지워진다** — 이 화면의 유일한
    /// 비-레지스트리 누출 경로(모듈 헤더 "시크릿" 2번).
    #[test]
    fn passwords_in_prose_uris_are_redacted() {
        let cases = [
            "connection failed: mongodb://backup:s3cr3t-pw@db1.internal:27017/admin timed out",
            "error at postgres://u:p%40ss@10.0.0.4:5432/app?sslmode=require",
            "mysql://root:hunter2@localhost/mysql",
            "(see mongodb+srv://a:b@cluster0.abc.mongodb.net/test)",
            "\"s3://key:secret@bucket/path\"",
        ];
        for raw in cases {
            let out = redact_uris(raw);
            for leak in ["s3cr3t-pw", "p%40ss", "hunter2", ":b@", "secret@"] {
                assert!(!out.contains(leak), "비밀번호가 남았다: {out}");
            }
            assert!(out.contains("***@"), "가림 흔적이 없다: {out}");
        }
        // 호스트는 보존된다 — "어디로 연결하려 했는가"는 진단에 필요하다.
        let out = redact_uris("connection failed: mongodb://u:p@db1.internal:27017/admin");
        assert!(
            out.contains("db1.internal:27017"),
            "호스트가 사라졌다: {out}"
        );
    }

    /// URI가 없는 텍스트·경계 케이스에서 원문이 망가지지 않는다.
    #[test]
    fn redaction_leaves_non_uri_text_alone() {
        for raw in [
            "",
            "no uri here",
            "ratio 3://4 is not a uri",
            "한글과 이모지 🙂 그리고 slash// 만 있는 문장",
            "writable (local: /srv/backups/prod), free 12.3 GiB",
            "://leading-separator-without-scheme",
        ] {
            assert_eq!(redact_uris(raw), raw, "원문이 바뀌었다: {raw}");
        }
        // 자격증명이 없는 URI는 그대로(스킴·호스트·경로 보존).
        assert_eq!(
            redact_uris("see mongodb://db1:27017/admin now"),
            "see mongodb://db1:27017/admin now"
        );
        // 한 문장에 여러 개가 있어도 전부 접힌다.
        let out = redact_uris("a mongodb://u1:p1@h1/db and b postgres://u2:p2@h2/db end");
        assert!(
            !out.contains("p1") && !out.contains("p2"),
            "일부만 접혔다: {out}"
        );
        assert!(out.ends_with(" end"), "뒤쪽 산문이 잘렸다: {out}");
        // 멀티바이트 뒤에 붙은 URI도 문자 경계를 깨지 않는다(패닉 없음).
        let out = redact_uris("연결 실패: mongodb://u:p@호스트없음/db 끝");
        assert!(!out.contains(":p@"), "{out}");
    }

    /// 접기는 멱등이다 — 뷰가 한 번 더 통과시켜도 안전하다([`scrub`] doc).
    #[test]
    fn scrubbing_is_idempotent() {
        let mut registry = SecretRegistry::new();
        assert!(registry.register("NOT-A-REAL-SECRET-dash-1a2b3c4d"));
        let raw = "auth NOT-A-REAL-SECRET-dash-1a2b3c4d via mongodb://u:pw@h/db";
        let once = scrub(&registry, raw);
        assert_eq!(scrub(&registry, &once), once, "두 번 통과시키면 달라진다");
        assert!(!once.contains("NOT-A-REAL-SECRET"), "{once}");
        assert!(!once.contains(":pw@"), "{once}");
    }

    // -- 수집(자식 실제 실행) ---------------------------------------------

    /// 프로파일 3개에서 연결·복제 지연·마지막 백업 나이·destination 여유가 전부 표시된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn three_profiles_render_connection_lag_age_and_free_space() {
        let (ctx, dir) = ctx_with_fake_exe("healthy");
        plant_healthy(&dir);
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다: {dashboard:?}");
        };
        assert_eq!(rows.len(), 3, "행 수가 다르다");
        assert_eq!(dashboard.level(), Level::Ok, "건강한 화면이 초록이 아니다");
        assert_eq!(dashboard.needs_attention(), 0);

        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        for (profile, engine, free) in [
            ("mongo-analytics", "mongodb", "12.3 GiB"),
            ("mysql-billing", "mysql", "480.0 GiB"),
            ("pg-staging", "postgresql", "1.9 TiB"),
        ] {
            assert!(html.contains(profile), "프로파일 누락: {profile}");
            assert!(html.contains(engine), "엔진 누락: {engine}");
            assert!(html.contains(free), "destination 여유 공간 누락: {free}");
        }
        assert!(html.contains("SCRAM-SHA-256"), "연결 칸 누락");
        assert!(html.contains("0.4s"), "복제 지연 칸 누락");
        assert!(html.contains("3h ago"), "마지막 백업 나이 누락");
        // 자식은 명부 1 + 프로파일 3 = 4번 떴다.
        assert_eq!(spawn_count(&dir), 4, "자식 수가 다르다");
    }

    /// **한 프로파일이 타임아웃해도 나머지가 렌더된다** — 이 화면의 존재 이유.
    #[cfg(unix)]
    #[tokio::test]
    async fn one_timing_out_profile_does_not_hide_the_others() {
        let (ctx, dir) = ctx_with_fake_exe("partial");
        plant_healthy(&dir);
        // 가운데 프로파일만 5초 잔다(상한 400ms).
        std::fs::write(dir.join("slow-mysql-billing"), b"1").unwrap();
        let cache = test_cache();

        let started = Instant::now();
        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(3),
            "느린 프로파일이 화면을 붙잡았다: {elapsed:?}"
        );

        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다: {dashboard:?}");
        };
        assert_eq!(rows.len(), 3, "타임아웃이 행을 없앴다");
        let slow = rows.iter().find(|r| r.profile == "mysql-billing").unwrap();
        assert!(
            matches!(&slow.state, RowState::Unavailable { .. }),
            "느린 프로파일이 진단으로 접히지 않았다: {:?}",
            slow.state
        );
        // 나머지 둘은 정상 보고다.
        for name in ["mongo-analytics", "pg-staging"] {
            let row = rows.iter().find(|r| r.profile == name).unwrap();
            assert!(
                matches!(
                    &row.state,
                    RowState::Reported {
                        level: Level::Ok,
                        ..
                    }
                ),
                "{name}이 함께 무너졌다: {:?}",
                row.state
            );
        }
        // 화면에도 셋 다 있고, 느린 행은 실패가 아니라 "점검 불가"로 표시된다.
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        for name in ["mongo-analytics", "mysql-billing", "pg-staging"] {
            assert!(html.contains(name), "{name}이 화면에서 사라졌다");
        }
        assert!(html.contains(r#"data-level="error""#), "진단 레벨 누락");
        assert!(html.contains("12.3 GiB"), "정상 행의 내용이 사라졌다");
    }

    /// **캐시 히트에는 자식이 뜨지 않는다** — t19의 핵심 단정(프로세스 수준 확인).
    #[cfg(unix)]
    #[tokio::test]
    async fn cache_hit_spawns_no_child() {
        let (ctx, dir) = ctx_with_fake_exe("cachehit");
        plant_healthy(&dir);
        let cache = test_cache();

        collect(&ctx, &cache, fast_limits()).await;
        let after_first = spawn_count(&dir);
        assert_eq!(after_first, 4, "첫 수집의 자식 수가 다르다");

        // 두 번째·세 번째 수집은 전부 캐시에서 나온다.
        for _ in 0..2 {
            let dashboard = collect(&ctx, &cache, fast_limits()).await;
            let Dashboard::Rows {
                rows, roster_hit, ..
            } = &dashboard
            else {
                panic!("명부를 못 읽었다");
            };
            assert!(roster_hit, "명부가 캐시에서 나오지 않았다");
            assert!(rows.iter().all(|r| r.hit), "행이 캐시에서 나오지 않았다");
            assert_eq!(rows.len(), 3);
        }
        assert_eq!(
            spawn_count(&dir),
            after_first,
            "캐시 히트인데 자식이 또 떴다 — 읽기 전용 명령이 프로덕션에 지속 부하를 만든다"
        );
    }

    /// **무효화 후에는 다시 뜬다** — 쓰기 작업이 끝난 뒤 화면이 갱신되는 기반.
    #[cfg(unix)]
    #[tokio::test]
    async fn invalidation_makes_the_next_load_spawn_again() {
        let (ctx, dir) = ctx_with_fake_exe("invalidate");
        plant_healthy(&dir);
        let cache = test_cache();

        collect(&ctx, &cache, fast_limits()).await;
        let baseline = spawn_count(&dir);

        // 그 프로파일의 백업이 끝났다고 가정하고 무효화한다.
        let config = cache::config_key(&ctx);
        cache.invalidate(&ProbeKey::Profile {
            config: config.clone(),
            profile: "mysql-billing".into(),
        });

        // 그 프로파일의 마지막 백업이 방금으로 바뀐 표본을 심는다.
        plant_status(
            &dir,
            "mysql-billing",
            &status_json("mysql-billing", "ok", "ok", "479.0 GiB").replace("3h ago", "2s ago"),
        );

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        assert_eq!(
            spawn_count(&dir),
            baseline + 1,
            "무효화한 프로파일 하나만 다시 떠야 한다"
        );
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(html.contains("2s ago"), "새 값이 화면에 반영되지 않았다");
        // 무효화하지 않은 프로파일은 그대로 캐시본이다.
        assert!(html.contains("12.3 GiB"), "옆 프로파일이 함께 지워졌다");
    }

    /// 전역 무효화 진입점이 이 화면의 캐시 키와 실제로 맞물린다 — 리더가 배선할 시그니처.
    #[cfg(unix)]
    #[tokio::test]
    async fn global_invalidation_entry_point_matches_this_screen_keys() {
        let (ctx, dir) = ctx_with_fake_exe("globalinv");
        plant_healthy(&dir);
        // 이 테스트만 전역 캐시를 쓴다(키에 이 테스트의 config 경로가 들어가므로 격리된다).
        let cache = cache::probe_cache();

        collect(&ctx, cache, fast_limits()).await;
        let baseline = spawn_count(&dir);
        collect(&ctx, cache, fast_limits()).await;
        assert_eq!(spawn_count(&dir), baseline, "대조군이 캐시 히트여야 한다");

        cache::invalidate_profile(&ctx, "pg-staging");
        collect(&ctx, cache, fast_limits()).await;
        assert_eq!(
            spawn_count(&dir),
            baseline + 1,
            "프로파일 무효화가 먹지 않았다"
        );

        cache::invalidate_config(&ctx);
        collect(&ctx, cache, fast_limits()).await;
        assert_eq!(
            spawn_count(&dir),
            baseline + 1 + 4,
            "config 무효화가 명부+전 프로파일을 다시 띄우지 않았다"
        );
    }

    /// 손상된 자식 출력이 패닉 없이 진단 화면이 된다(빈 출력 포함).
    #[cfg(unix)]
    #[tokio::test]
    async fn corrupt_and_empty_child_output_render_diagnostics() {
        let (ctx, dir) = ctx_with_fake_exe("corrupt");
        plant_healthy(&dir);
        // 하나는 쓰레기, 하나는 빈 출력(표본 파일을 아예 두지 않는다 → stderr만 남는다).
        plant_status(
            &dir,
            "mongo-analytics",
            "{not json\u{0}\u{1}<script>x</script>",
        );
        std::fs::remove_file(dir.join("status-pg-staging.json")).unwrap();
        std::fs::write(dir.join("exit-pg-staging"), b"2").unwrap();
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다");
        };
        assert_eq!(rows.len(), 3);
        for name in ["mongo-analytics", "pg-staging"] {
            let row = rows.iter().find(|r| r.profile == name).unwrap();
            assert!(
                matches!(&row.state, RowState::Unreadable { .. }),
                "{name}: 진단으로 접히지 않았다: {:?}",
                row.state
            );
            assert_eq!(row.level(), Level::Error);
        }
        // 멀쩡한 프로파일은 그대로다.
        let ok = rows.iter().find(|r| r.profile == "mysql-billing").unwrap();
        assert_eq!(ok.level(), Level::Ok);

        // 렌더가 패닉하지 않고, 손상된 출력의 어떤 조각도 활성 마크업으로 나가지 않는다.
        //
        // `<script>`가 화면에 아예 도달하지 않는 것에 유의: serde의 오류 메시지는 입력을
        // 되돌려 싣지 않고("expected value at line 1 column 2") stderr도 비어 있다.
        // 즉 이 경로는 **반사 자체가 없다.** 반사가 있는 경로의 이스케이프는
        // `view::dashboard`의 `hostile_strings_are_escaped_and_do_not_break_rendering`가
        // 값을 직접 심어 확인한다.
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(!html.contains("<script>"), "스크립트 태그가 살아 있다");
        assert!(!html.contains('\u{0}'), "제어문자가 그대로 나갔다");
        assert!(html.contains("Dashboard"), "화면이 비었다");
    }

    /// 자식을 아예 띄울 수 없으면 화면 전체가 명부 오류가 된다(패닉·500 없음).
    #[tokio::test]
    async fn missing_executable_becomes_a_roster_error_page() {
        let mut ctx = ServeConfig::for_test();
        ctx.config_path = Some(PathBuf::from("/nonexistent/dash-missing-exe.toml"));
        ctx.jobs = Arc::new(JobRunner::with_exe(
            PathBuf::from("/nonexistent/x-backup-does-not-exist-5d81"),
            ctx.config_path.clone(),
            Lang::En,
            JobSecrets::new(),
        ));
        let ctx = Arc::new(ctx);
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        assert!(
            matches!(&dashboard, Dashboard::NoRoster { .. }),
            "명부 실패가 다른 상태로 접혔다: {dashboard:?}"
        );
        assert_eq!(dashboard.level(), Level::Error);
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(
            html.starts_with("<div") || html.contains("verdict"),
            "화면이 비었다"
        );
        assert!(
            !html.contains("/nonexistent/dash-missing-exe.toml"),
            "config 경로 노출"
        );
    }

    /// 프로파일이 0개인 config에서도 화면이 살아 있고, 초록이 아니다.
    #[cfg(unix)]
    #[tokio::test]
    async fn empty_roster_renders_a_notice_and_is_not_green() {
        let (ctx, dir) = ctx_with_fake_exe("emptyroster");
        std::fs::write(
            dir.join("roster.json"),
            br#"{"schema":1,"overall":"ok","profiles":[]}"#,
        )
        .unwrap();
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다");
        };
        assert!(rows.is_empty());
        assert_eq!(dashboard.level(), Level::Warn, "프로파일 0개가 초록이다");
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(html.contains("No profiles"), "안내 문장 누락: {html}");
    }

    /// 예산이 끝나면 남은 프로파일이 "미측정"으로 표시되고, 그 행이 실패가 아니다.
    #[cfg(unix)]
    #[tokio::test]
    async fn exhausted_budget_marks_remaining_profiles_as_unchecked() {
        let (ctx, dir) = ctx_with_fake_exe("budget");
        plant_healthy(&dir);
        // 첫 배치의 프로파일 하나를 느리게 만들고, 동시 폭을 1로 줄여 배치를 3번으로 만든다.
        std::fs::write(dir.join("slow-mongo-analytics"), b"1").unwrap();
        let limits = ProbeLimits {
            roster: Duration::from_secs(5),
            profile: Duration::from_millis(300),
            // 첫 프로파일이 상한(300ms)을 태우면 예산이 끝난다.
            budget: Duration::from_millis(150),
            concurrency: 1,
        };
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, limits).await;
        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다");
        };
        assert_eq!(rows.len(), 3, "예산 초과가 행을 없앴다");
        // 뒤쪽 두 프로파일은 자식이 뜨지 않았다(명부 1 + 첫 프로파일 1 = 2번).
        assert_eq!(spawn_count(&dir), 2, "예산을 넘겼는데 자식이 더 떴다");
        for name in ["mysql-billing", "pg-staging"] {
            let row = rows.iter().find(|r| r.profile == name).unwrap();
            let RowState::Unavailable { headline, detail } = &row.state else {
                panic!("{name}: 미측정이 아니다: {:?}", row.state);
            };
            assert!(headline.contains("Not checked"), "{headline}");
            assert!(
                detail.contains("not a failure"),
                "미측정이 실패로 읽힌다: {detail}"
            );
        }
    }

    /// 검증을 통과하지 못하는 프로파일 이름은 자식을 띄우지 않고 그 행만 진단이 된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn hostile_profile_name_is_refused_without_spawning() {
        let (ctx, dir) = ctx_with_fake_exe("badname");
        std::fs::write(
            dir.join("roster.json"),
            br#"{"schema":1,"overall":"ok","profiles":[
                {"profile":"../../etc/passwd","db":null,"items":[]},
                {"profile":"--force","db":null,"items":[]},
                {"profile":"mysql-billing","db":"mysql","items":[]}]}"#,
        )
        .unwrap();
        plant_healthy(&dir);
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let Dashboard::Rows { rows, .. } = &dashboard else {
            panic!("명부를 못 읽었다");
        };
        assert_eq!(rows.len(), 3);
        // 명부 1 + 정상 프로파일 1 = 2. 거부된 이름으로는 자식이 뜨지 않는다.
        assert_eq!(spawn_count(&dir), 2, "거부한 이름으로 자식이 떴다");
        for bad in ["../../etc/passwd", "--force"] {
            let row = rows.iter().find(|r| r.profile == bad).unwrap();
            assert!(matches!(&row.state, RowState::Unavailable { .. }));
        }
        // 이름이 마크업에 이스케이프되어 들어간다(반사 자체는 config에서 온 값이다).
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(!html.contains("<script"), "{html}");
    }

    /// 마스킹: 심어 둔 시크릿과 인라인 URI 비밀번호가 화면에서 지워진다.
    #[cfg(unix)]
    #[tokio::test]
    async fn secrets_and_inline_uris_are_masked_in_the_page() {
        const FAKE: &str = "NOT-A-REAL-SECRET-dashboard-7e41c9a2";
        let (ctx, dir) = ctx_with_fake_exe("mask");
        plant_healthy(&dir);
        // 자식이 시크릿 값과 인라인 URI를 함께 뱉는 상황을 만든다.
        let leaky = status_json("mongo-analytics", "fail", "fail", "12.3 GiB").replace(
            "connected (user=backup, mechanism=SCRAM-SHA-256)",
            &format!(
                "connection failed: mongodb://backup:inline-pw-not-in-registry@db1.internal:27017/admin (key {FAKE})"
            ),
        );
        plant_status(&dir, "mongo-analytics", &leaky);

        // 러너 레지스트리에 그 값을 등록한 컨텍스트를 만든다(실제로는 config의 `*_env`가 채운다).
        let mut secrets = JobSecrets::new();
        secrets.insert_value("XB_TEST_DASHBOARD_URI", FAKE);
        let mut cfg = (*ctx).clone();
        cfg.jobs = Arc::new(JobRunner::with_exe(
            dir.join("fake-x-backup"),
            cfg.config_path.clone(),
            Lang::En,
            secrets,
        ));
        let ctx = Arc::new(cfg);
        let cache = test_cache();

        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let registry = request_registry(&ctx, Some(&Arc::clone(&auth_and_session().0)));
        let html = view::body(Lang::En, true, &dashboard, &registry).into_string();

        assert!(!html.contains(FAKE), "레지스트리 시크릿이 화면에 남았다");
        assert!(
            html.contains(crate::web::mask::REDACTED_PLACEHOLDER),
            "마스킹 표식이 없다"
        );
        assert!(
            !html.contains("inline-pw-not-in-registry"),
            "레지스트리가 모르는 URI 비밀번호가 화면에 남았다"
        );
        assert!(
            html.contains("db1.internal"),
            "진단에 필요한 호스트가 사라졌다"
        );
        assert!(!html.contains(TEST_TOKEN), "세션 토큰이 화면에 남았다");
    }

    /// 화면이 서버 내부 경로(state 디렉터리·config 경로)를 흘리지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn markup_does_not_leak_server_paths() {
        let (ctx, dir) = ctx_with_fake_exe("paths");
        plant_healthy(&dir);
        let cache = test_cache();
        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();

        assert!(
            !html.contains(&ctx.state_dir.display().to_string()),
            "state 경로가 노출됐다"
        );
        assert!(
            !html.contains(&ctx.config_path.as_ref().unwrap().display().to_string()),
            "config 경로가 노출됐다"
        );
        assert!(!html.contains("fake-x-backup"), "실행 파일 경로가 노출됐다");
        // config는 존재 여부만 말한다.
        assert!(html.contains("wired"), "config 연결 표시 누락");
    }

    // -- 라우팅·인증 -------------------------------------------------------

    /// 쿠키 없이는 401이고 화면 내용을 흘리지 않는다. 쿠키가 있으면 200 + 껍데기다.
    #[cfg(unix)]
    #[tokio::test]
    async fn screen_requires_auth_and_renders_with_a_cookie() {
        let (ctx, dir) = ctx_with_fake_exe("auth");
        plant_healthy(&dir);

        let (status, body) = call(Arc::clone(&ctx), None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "화면이 열려 있다");
        assert!(
            !body.contains(DASHBOARD_TITLE),
            "미인증 응답이 화면을 흘렸다"
        );
        assert!(
            !body.contains("mongo-analytics"),
            "미인증 응답이 프로파일을 흘렸다"
        );
        assert!(!body.contains(TEST_TOKEN), "토큰이 응답에 노출됐다");
        assert_eq!(spawn_count(&dir), 0, "미인증 요청이 자식을 띄웠다");

        // 틀린 쿠키도 401 — 쿠키가 "있기만" 하면 통과하는 사고를 막는다.
        let (status, _) = call(Arc::clone(&ctx), Some("not-the-token")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(spawn_count(&dir), 0);

        let (status, body) = call(ctx, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("<!DOCTYPE html>"), "문서 골격 누락");
        assert!(
            body.contains("<title>x-backup — Dashboard</title>"),
            "화면 제목 누락"
        );
        assert!(body.contains("mongo-analytics"), "행이 렌더되지 않았다");
    }

    /// 렌더 결과에 색 리터럴·인라인 스타일이 없다 — 모양은 전부 CSS 몫이다.
    #[cfg(unix)]
    #[tokio::test]
    async fn markup_carries_no_presentation() {
        let (ctx, dir) = ctx_with_fake_exe("presentation");
        plant_healthy(&dir);
        std::fs::write(dir.join("exit-pg-staging"), b"3").unwrap();
        let cache = test_cache();
        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();

        assert!(!html.contains("style="), "인라인 스타일이 들어갔다");
        assert!(!html.contains('#'), "색 리터럴로 보이는 값이 있다");
        assert!(
            html.contains("data-level="),
            "상태가 data-level로 나가지 않는다"
        );
    }

    /// **전체 렌더 시간 측정** — NFR(p95 < 1.5s)의 관측 지점.
    ///
    /// 가짜 자식(셸 스크립트 + `cat`)이므로 이 수치는 실제 DB 지연을 포함하지 않는다.
    /// 그래서 이 테스트가 재는 것은 **콘솔이 더하는 비용**이다: 프로세스 생성·파이프
    /// 수집·파싱·마크업 생성. 실제 DB 지연은 프로파일별 상한이 덮는다.
    ///
    /// 상한을 넉넉히(2초) 잡은 이유는 CI가 느린 공유 러너일 수 있기 때문이다 —
    /// 이 테스트의 목적은 성능 회귀 경보이지 벤치마크가 아니다. 실측치는 보고에 적었다.
    #[cfg(unix)]
    #[tokio::test]
    async fn cold_and_warm_page_timing_is_recorded() {
        let (ctx, dir) = ctx_with_fake_exe("timing");
        plant_healthy(&dir);
        let cache = test_cache();

        let cold_start = Instant::now();
        let dashboard = collect(&ctx, &cache, fast_limits()).await;
        let cold_collect = cold_start.elapsed();
        let render_start = Instant::now();
        let html = view::body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        let render = render_start.elapsed();

        let warm_start = Instant::now();
        let warm_dashboard = collect(&ctx, &cache, fast_limits()).await;
        let warm_collect = warm_start.elapsed();
        let _ = view::body(Lang::En, true, &warm_dashboard, &empty_registry()).into_string();

        // 눈으로 볼 수 있게 남긴다(`cargo test -- --nocapture`).
        println!(
            "dashboard timing — cold collect {cold_collect:?}, render {render:?}, warm collect {warm_collect:?}, html {} bytes",
            html.len()
        );
        assert!(
            cold_collect < Duration::from_secs(2),
            "콜드 수집이 너무 느리다: {cold_collect:?}"
        );
        assert!(
            render < Duration::from_millis(50),
            "렌더가 너무 느리다(순수 함수인데): {render:?}"
        );
        assert!(
            warm_collect < cold_collect,
            "캐시가 warm 경로를 빠르게 만들지 못했다: cold {cold_collect:?} warm {warm_collect:?}"
        );
    }

    /// 운영 기본 상한이 서로 모순되지 않는다 — 값이 흔들려도 관계는 유지되어야 한다.
    #[test]
    fn default_limits_are_internally_consistent() {
        let l = ProbeLimits::default();
        assert!(
            l.concurrency >= 1,
            "동시 폭이 0이면 아무것도 프로브하지 않는다"
        );
        assert!(
            l.profile < l.budget,
            "프로파일 상한이 예산보다 크면 첫 프로파일이 예산을 통째로 태운다"
        );
        // 명부 + 예산이 앞단 프록시 기본 읽기 상한(nginx 60초)보다 넉넉히 짧아야
        // 우리가 만든 화면이 나가고 프록시의 504 기본 페이지가 나가지 않는다.
        assert!(
            l.roster + l.budget <= Duration::from_secs(30),
            "최악 지연이 프록시 상한에 너무 가깝다"
        );
        // TTL은 프로브 상한보다 길어야 의미가 있다(값이 만들어지기도 전에 만료되면 안 된다).
        assert!(PROBE_TTL > l.profile, "TTL이 프로파일 상한보다 짧다");
    }
}
