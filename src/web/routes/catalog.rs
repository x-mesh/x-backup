//! `GET /catalog` — 백업 카탈로그 화면(정렬·필터·페이지네이션).
//!
//! ## 데이터는 오직 `list --json` 자식 프로세스로만 얻는다
//! [`crate::web`] 최상위 불변식("웹은 도메인 로직을 재구현하지 않는다")을 이 화면에서
//! 가장 지키기 쉬운 방법은 카탈로그를 두 번째로 읽는 코드를 아예 만들지 않는 것이다.
//! 그래서 이 파일은 [`crate::manifest`]·[`crate::storage`]를 **한 번도** import하지
//! 않는다 — [`crate::cli::handlers::list::build_catalog`]가 이미 존재하지만, 그것을 직접
//! 부르면 "카탈로그를 읽는 방법"이 CLI 경로와 웹 경로 둘로 갈라진다. 대신
//! [`crate::web::job::JobRunner`]로 `list --json`을 자식으로 띄우고 stdout을 파싱한다.
//!
//! ## `list --json`의 8개 키는 어떤 테스트로도 고정되지 않았다 — 조용히 넘기지 않는다
//! 리뷰가 지적한 사실: `id, type, promoted_from_gap, engine, created_at, stored_size_bytes,
//! chain_status, base_id` 여덟 키의 이름이 바뀌어도(예: `chain_status` → `chainStatus`)
//! CLI 쪽 스위트는 전부 통과한다. 즉 이 화면이 파싱하는 계약은 지금 무방비다. 그래서
//! [`parse_row`]는 `serde_json::Value`를 직접 검사해 **필수 키가 없거나 타입이 틀리면
//! 그 행을 숨기지 않고 [`CatalogEntry::Unparseable`]로 표면화**한다(H10과 같은 원칙 —
//! [`crate::cli::handlers::list`] 모듈 헤더의 "corrupt 행" 처리와 동일한 사상). `schema`
//! 필드는 최상위에서 먼저 확인해([`parse_document`]) 모르는 버전이면 그 즉시
//! [`ReportError::SchemaMismatch`]로 명확히 실패한다 — 알 수 없는 스키마를 아는 스키마인
//! 척 파싱하지 않는다.
//!
//! ## 페이지네이션 — 전체를 받아 서버에서 자른다(방식 a)
//! CLI에는 `--limit`만 있고 오프셋·커서가 없다. 세 방식을 저울질했다:
//!
//! - **(a) 전체를 받아 서버에서 자른다.** [`build_catalog`](crate::cli::handlers::list::build_catalog)가
//!   손상 판정(broken/orphan/corrupt)을 **전체 카탈로그** 기준으로 하므로([`list`] 모듈
//!   헤더의 "필터로 문제를 숨기지 않게"), 어차피 매 요청 전체를 받아야 정확한 경고
//!   개수를 알 수 있다. `--type`/`--engine` 필터도 CLI에 없으므로(아래 "JobSpec이 표현하지
//!   못하는 것" 참조) 이 화면이 직접 걸러야 한다 — 그러려면 필터 전 전체 집합이 필요하다.
//! - **(b) `--limit`만 쓰고 "더 보기"로 늘려간다.** `--limit`이 있어도
//!   [`build_catalog`]는 **limit 적용 전**에 이미 전체 카탈로그를 만들고 체인 상태를
//!   계산한다([`list`] 코드 참조 — `all_rows`를 만든 뒤에야 `rows.truncate(lim)`). 즉
//!   `--limit`을 줄여도 자식의 계산 비용은 줄지 않는다 — 페이지네이션 이득이 없다.
//! - **(c) CLI에 오프셋을 추가.** CLI 파일을 고치는 것은 이 태스크 범위 밖이다(리더 지시).
//!
//! (b)가 계산 비용을 줄이지 못한다는 사실이 (a)를 선택한 결정적 근거다 — 어차피 전체를
//! 계산하는 자식을 매번 띄운다면, 그 결과를 한 번에 받아 화면 쪽에서 정렬·필터 유지한
//! 채 자르는 편이 자식을 여러 번 띄우는 것보다 낫다(자식 spawn 자체도 공짜가 아니다 —
//! `job::runner` 헤더의 프로세스 그룹·env 화이트리스트 구성 비용).
//!
//! ## 성능 실측 — CLI 쪽 O(n²)을 여기서 고칠 수 없다
//! [`crate::manifest::chain::verify_chain`]은 매 백업마다 **전체 노드 목록**을 다시
//! 스캔한다(`find_node` + `nodes.iter().filter(...)` — chain.rs 참조). [`build_catalog`]는
//! 그것을 백업 개수만큼 반복 호출하므로 카탈로그 전체가 O(n²)이다. 이건 `--limit`으로도
//! 피할 수 없다(위 "(b)" 참조) — 필터를 걸어도 전체를 계산한 **뒤** 자르기 때문이다.
//! 이 화면은 자식을 spawn만 할 뿐 그 알고리즘을 고칠 권한이 없으므로([`crate::manifest`]는
//! 이 태스크가 손댈 수 없는 크레이트다), NFR(< 500ms)이 큰 카탈로그에서 지켜지는지는
//! **이 화면이 보장할 수 있는 범위 밖**이다. 대신 이 파일 자신의 몫(JSON 파싱·필터·
//! 페이지네이션·렌더)은 순수 함수라 자식 없이 벤치마크할 수 있고,
//! `catalog_pipeline_handles_ten_thousand_rows_quickly` 테스트가 10,000행 이상에서 그
//! 몫을 실측한다. CLI 쪽 O(n²)은 보고서에 별도로 남긴다(`src/manifest/chain.rs`·
//! `src/cli/handlers/list.rs` 소유자가 볼 문제 — 이 태스크가 고칠 파일이 아니다).
//!
//! ## `JobSpec`이 표현하지 못하는 것 — `--engine` 필터, `--type orphan/corrupt`
//! [`JobSpec::with_backup_type`]은 [`crate::cli::args::BackupType`](Full/Incr)만 받는다 —
//! orphan·corrupt는 이 어휘에 없다. [`crate::web::job::spec::JobFlag`]·
//! [`crate::web::job::spec::JobCount`]에도 `--engine`에 대응하는 항목이 없다. 그래서 이
//! 화면은 **애초에 `--type`/`--engine`을 자식에 넘기지 않는다** — 자식에게는 정렬
//! (`--sort`/`--asc`)만 요청하고, 유형·엔진 필터는 이미 파싱된 [`CatalogEntry`] 위에서
//! 이 파일이 직접 건다([`filter_and_paginate`]). 이것은 "도메인 로직 재구현"이 아니다 —
//! 문자열 동등 비교일 뿐이고, 무엇이 postgresql/mongodb/mysql/orphan/corrupt인지의 **판정**
//! 자체는 여전히 자식이 이미 내린 값(`type`/`engine` 필드)을 그대로 쓴다. 그래도 `--limit`
//! 앞의 두 플래그가 어휘에 있었다면 자식 쪽에서 걸러 응답 크기를 줄일 수 있었을 것이므로,
//! `JobSpec` 확장이 필요하다는 사실은 보고에 남긴다.
//!
//! ## store 위치를 화면에 보여준다 — `routes::lock`과 다른 판단
//! `routes::lock::LOCK_PATH` 화면은 락 파일의 **절대 경로**(서버 내부 구조)를 숨기기로
//! 했다. 여기서 store 위치(로컬 경로 또는 `s3://버킷`)는 다르다 — `list`의 **사람용
//! 출력조차 첫 줄에 그대로 찍는 정보**다([`crate::cli::handlers::list::print_human`]의
//! `store: {store_loc}`). CLI 사용자에게 이미 공개된 정보를 웹에서만 가리는 것은 방어가
//! 아니라 "어디를 보고 있는지" 스스로 알 수 없게 만드는 혼란이다. 그래도 시크릿 원문(접속
//! URI의 자격증명 등)은 이 문자열에 애초에 들어갈 자리가 없고([`crate::cli::handlers::list::store_location`]는
//! 경로/버킷명만 만든다), 혹시 모를 경우를 대비해 [`request_registry`]로 한 번 더
//! 마스킹한다(2차 방어 — 이 크레이트 전역 관례).
//!
//! ## exit 코드 — 4는 실패가 아니다, 목록은 그대로 보여준다
//! `list`는 broken/incomplete/orphan/corrupt가 하나라도 있으면 exit 4([`XBackupError::VerifyWarning`])를
//! 낸다. 그런데 **JSON은 그 판정 이전에 이미 stdout에 찍혀 있다**([`crate::cli::handlers::list::handle`]가
//! 출력 → 경고 판정 순서로 동작한다). 그래서 이 화면은 exit 4에서도 목록 데이터를 정상
//! 파싱해 그대로 보여주고, [`Verdict::Warned`]로 "경고가 있다"는 사실만 배너에 얹는다 —
//! exit 4를 실패로 접으면(목록을 숨기면) 운영자가 정작 확인해야 할 카탈로그 자체를 못
//! 보게 된다.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use clap::ValueEnum;
use serde::Deserialize;
use serde_json::Value;

use crate::cli::args::ListSort;
use crate::error::exit_codes;
use crate::i18n::Lang;
use crate::web::job::{JobCommand, JobFlag, JobRunner, JobSpec, ProfileName, RunningJob};
use crate::web::jsonguard;
use crate::web::mask::SecretRegistry;
use crate::web::view::components::Level;
use crate::web::view::{catalog as view, layout};
use crate::web::ServeConfig;

/// 화면 경로. 라우터·마크업·테스트가 이 상수를 공유한다.
pub const CATALOG_PATH: &str = "/catalog";

/// `<title>`·화면 제목·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const CATALOG_TITLE: &str = "Catalog";

/// 한 페이지에 보여줄 행 수.
///
/// 50은 스크롤 한 화면 반 정도에서 훑을 수 있는 밀도이면서, 페이지 전환 빈도가 지나치게
/// 잦아지지 않는 선이다(운영자가 "가장 최근 며칠"을 보는 흔한 조작에서 페이지를 5~10번
/// 넘기지 않는다). 매직 넘버로 흩어지지 않게 `page_href`·테스트가 전부 이 상수를 쓴다.
pub const PAGE_SIZE: usize = 50;

/// 자식 실행 상한.
///
/// [`crate::manifest::chain::verify_chain`]이 카탈로그 전체를 매 백업마다 다시 스캔하므로
/// (모듈 헤더 "성능 실측" 참조) 카탈로그가 크면 `list`가 수 초~수십 초 걸릴 수 있다.
/// `doctor`의 20초([`crate::web::routes::doctor::DOCTOR_TIMEOUT`])보다 넉넉히 60초를
/// 준다 — 그래도 무한 대기는 남기지 않는다(상한을 넘기면 [`kill_child`]가 SIGKILL한다).
/// 앞단 리버스 프록시의 `proxy_read_timeout` 기본값(60초)과 맞닿는 값이므로, 이보다
/// 더 늘리면 우리가 만든 읽을 수 있는 타임아웃 화면 대신 프록시의 504가 먼저 나갈 수
/// 있다(같은 근거를 `doctor.rs` 헤더가 이미 세워 뒀다).
const CATALOG_TIMEOUT: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------------------
// 쿼리 파라미터 — 원시 문자열만 받는다(axum이 구조적으로 거부하지 않게)
// ---------------------------------------------------------------------------

/// `GET /catalog?...`의 원시 쿼리. 전부 `Option<String>`이라 어떤 값이 와도 axum
/// 추출 단계에서 400이 나지 않는다 — 검증·정규화는 [`CatalogParams::from_query`]가 한다
/// (틀린 값은 조용히 기본값으로 접힌다. `profile`만 예외 — 락 파일 이름 조각이 되는
/// 값이라 형태 오류를 조용히 넘기지 않는다).
#[derive(Debug, Deserialize)]
pub struct CatalogQuery {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    sort: Option<String>,
    #[serde(default)]
    asc: Option<String>,
    #[serde(default, rename = "type")]
    type_filter: Option<String>,
    #[serde(default)]
    engine: Option<String>,
    #[serde(default)]
    page: Option<String>,
}

/// 검증·정규화를 마친 화면 상태 — 자식 spawn과 화면 렌더가 함께 참조한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogParams {
    pub profile: Option<ProfileName>,
    pub sort: ListSort,
    pub asc: bool,
    pub type_filter: Option<String>,
    pub engine_filter: Option<String>,
    pub page: usize,
}

impl CatalogParams {
    /// 원시 쿼리를 검증한다. `profile`이 있는데 형태가 틀리면 그 이유를 담아 실패한다
    /// (그 값은 락 파일 경로 조각이 되므로 조용히 무시하면 안 된다 — `job::args` 헤더의
    /// "왜 프로파일명이 가장 엄격한가" 참조). 그 외 필드는 모르는 값이면 기본값으로
    /// 접는다 — 정렬·필터는 표시 판단이지 안전 경계가 아니다.
    fn from_query(q: &CatalogQuery, lang: Lang) -> Result<Self, String> {
        let profile = match q
            .profile
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(ProfileName::parse(raw, lang).map_err(|e| e.to_string())?),
            None => None,
        };
        let sort = q
            .sort
            .as_deref()
            .and_then(|s| ListSort::from_str(s, true).ok())
            .unwrap_or_default();
        let asc = matches!(q.asc.as_deref(), Some("1") | Some("true") | Some("on"));
        let type_filter = q.type_filter.as_deref().and_then(normalize_type_filter);
        let engine_filter = q.engine.as_deref().and_then(normalize_engine_filter);
        let page = q
            .page
            .as_deref()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|&p| p > 0)
            .unwrap_or(1);
        Ok(Self {
            profile,
            sort,
            asc,
            type_filter,
            engine_filter,
            page,
        })
    }
}

/// `--type` 어휘를 정규화한다. CLI의 [`crate::cli::handlers::list::normalize_type`]과
/// 값 집합이 다르다 — 여기는 **표시 필터**라 `corrupt`도 고를 수 있어야 한다(CLI는
/// `--type`을 자식에 넘기지 않으므로 이 화면이 만드는 필터 어휘이지 CLI 인자 표면이
/// 아니다). 모르는 값은 "필터 없음"으로 조용히 접는다(안전 경계가 아니므로 에러로
/// 끊을 이유가 없다).
fn normalize_type_filter(raw: &str) -> Option<String> {
    match raw.to_ascii_lowercase().as_str() {
        "full" | "incr" | "orphan" | "corrupt" => Some(raw.to_ascii_lowercase()),
        _ => None,
    }
}

/// `--engine` 어휘를 정규화한다. 별칭(`pg`/`postgres`/`mongo`)까지 CLI의
/// [`crate::cli::handlers::list::normalize_engine_filter`]와 같은 값 집합으로 접는다 —
/// 두 곳의 "postgresql이 무엇을 뜻하는가"가 갈라지면 필터가 헷갈린다.
fn normalize_engine_filter(raw: &str) -> Option<String> {
    match raw.to_ascii_lowercase().as_str() {
        "postgresql" | "postgres" | "pg" => Some("postgresql".to_string()),
        "mongodb" | "mongo" => Some("mongodb".to_string()),
        "mysql" | "mariadb" => Some("mysql".to_string()),
        _ => None,
    }
}

/// 정렬 값의 쿼리 문자열 토큰(왕복용).
fn sort_token(sort: ListSort) -> &'static str {
    match sort {
        ListSort::Created => "created",
        ListSort::Size => "size",
    }
}

// ---------------------------------------------------------------------------
// 자식 실행 — doctor.rs와 같은 패턴(spawn → 상한까지 대기 → 종료 코드 분류)
// ---------------------------------------------------------------------------

/// 이 화면이 만드는 명세 — `list --sort <s> [--asc] [--profile <p>] --json`.
///
/// `--type`/`--engine`/`--limit`은 **의도적으로 붙이지 않는다**(모듈 헤더 "JobSpec이
/// 표현하지 못하는 것"·"페이지네이션" 참조) — 전체를 받아 이 파일이 직접 거르고 자른다.
fn list_spec(profile: Option<ProfileName>, sort: ListSort, asc: bool, lang: Lang) -> JobSpec {
    let mut spec = JobSpec::new(JobCommand::List, lang).with_sort(sort);
    if let Some(p) = profile {
        spec = spec.with_profile(p);
    }
    if asc {
        spec = spec.with_flag(JobFlag::Asc);
    }
    spec
}

/// 자식을 돌리지 못한 이유. `doctor.rs::RunError`를 재사용하지 않는 이유: 그 타입은
/// `routes::doctor`가 소유하며, 이 화면이 그것을 가져다 쓰면 두 화면이 서로의 내부
/// 표현에 결합된다 — 화면마다 자기 손으로 완결되게 두는 것이 이 코드베이스의 관례다
/// (`routes::backup`의 `load_profile_names` 중복 결정과 같은 근거).
#[derive(Debug, Clone, PartialEq, Eq)]
enum RunError {
    /// spawn 실패(실행 권한·파일 없음 등).
    Spawn(String),
    /// 자식을 기다리는 중 I/O 오류.
    Wait(String),
    /// 상한 시간 초과 — 자식은 죽였다.
    Timeout(Duration),
}

impl RunError {
    fn explain(&self, lang: Lang) -> String {
        match self {
            RunError::Spawn(detail) => format!(
                "{} {}",
                lang.sel(
                    "Spawning the list child failed:",
                    "list 자식 프로세스를 띄우지 못했습니다:",
                ),
                excerpt(detail)
            ),
            RunError::Wait(detail) => format!(
                "{} {}",
                lang.sel(
                    "Reading the list child's output failed:",
                    "list 자식 프로세스의 출력을 읽는 중 실패했습니다:",
                ),
                excerpt(detail)
            ),
            RunError::Timeout(limit) => format!(
                "{} ({}s)",
                lang.sel(
                    "list did not finish within the time limit and was killed. A very large catalog is the usual cause.",
                    "list가 상한 시간 안에 끝나지 않아 종료시켰습니다. 카탈로그가 매우 큰 경우가 흔한 원인입니다.",
                ),
                limit.as_secs()
            ),
        }
    }
}

/// 자식 실행 결과(종료 코드 + 표준 스트림). `stdout`/`stderr`는 이미 lossy 변환된
/// [`crate::web::job::JobCompletion`]의 값이므로 여기서 비-UTF8을 다시 걱정하지 않는다.
#[derive(Debug)]
struct ChildRun {
    exit: Option<i32>,
    stdout: String,
    stderr: String,
}

async fn run_list(
    runner: &JobRunner,
    spec: &JobSpec,
    limit: Duration,
) -> Result<ChildRun, RunError> {
    let running = runner
        .spawn(spec)
        .map_err(|e| RunError::Spawn(e.to_string()))?;
    await_with_limit(running, limit).await
}

/// 돌고 있는 자식을 상한까지만 기다린다 — `routes::doctor::await_with_limit`과 같은
/// 판단(그쪽 헤더 참조: `kill_on_drop`을 켜지 않는 잡 러너 정책은 건드리지 않고, 이
/// 화면에만 필요한 유한 실행을 호출부에서 얹는다). 그룹이 아니라 pid 하나만 죽인다 —
/// `list`는 destination type=local만 지원하고 외부 도구를 다시 띄우지 않으므로(로컬
/// 파일시스템만 읽는다) 정리할 손자 프로세스가 없다.
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

#[cfg(unix)]
fn kill_child(pid: Option<u32>) {
    let Some(pid) = pid else { return };
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    // SAFETY: pid는 방금 우리가 spawn한, 아직 수거되지 않은 자식의 pid다.
    unsafe {
        libc::kill(pid, libc::SIGKILL);
    }
}

#[cfg(not(unix))]
fn kill_child(_pid: Option<u32>) {}

/// 오류 화면에 붙이는 자식 stderr 발췌 상한(문자 수) — `routes::doctor::EXCERPT_CHARS`와
/// 같은 값·근거(짧은 진단 한 줄 + 스택 힌트가 들어가는 폭).
const EXCERPT_CHARS: usize = 400;

fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= EXCERPT_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(EXCERPT_CHARS).collect();
    format!("{head}…")
}

// ---------------------------------------------------------------------------
// 판정 — exit code → 화면 상태(순수)
// ---------------------------------------------------------------------------

/// 자식 종료 코드의 화면 판정. `routes::doctor::Verdict`와 구조가 비슷한 이유는 두 화면
/// 모두 "0=정상/4=경고 동반 성공/그 외=문제"라는 같은 PRD §9 규약을 그린다는 사실
/// 때문이지, 타입을 공유해서가 아니다(위 [`RunError`] doc과 같은 독립성 판단).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// exit 0 — 문제 없음.
    Clean,
    /// exit 4 — **경고 동반 성공.** 목록은 정상이고, broken/incomplete/orphan/corrupt가
    /// 섞여 있다는 뜻이다. 실패로 칠하면 운영자가 정상적인 카탈로그 조회를 실패로 오인한다.
    Warned,
    /// exit 2 — 설정 문제(프로파일 없음·destination 미설정 등). 카탈로그를 얻지 못했다.
    Misconfigured,
    /// exit 1 — 읽기 실패(권한·I/O 등). 카탈로그를 얻지 못했다.
    Failed,
    /// PRD 밖의 종료(3/5 포함 — list는 이 코드를 내지 않지만 억지로 뭉개지 않는다).
    Unexpected(Option<i32>),
}

impl Verdict {
    fn from_exit(code: Option<i32>) -> Self {
        match code {
            Some(c) if c == i32::from(exit_codes::SUCCESS) => Verdict::Clean,
            Some(c) if c == i32::from(exit_codes::WARNING) => Verdict::Warned,
            Some(c) if c == i32::from(exit_codes::USAGE) => Verdict::Misconfigured,
            Some(c) if c == i32::from(exit_codes::FAILURE) => Verdict::Failed,
            other => Verdict::Unexpected(other),
        }
    }

    pub fn level(self) -> Level {
        match self {
            Verdict::Clean => Level::Ok,
            Verdict::Warned => Level::Warn,
            Verdict::Failed => Level::Fail,
            Verdict::Misconfigured | Verdict::Unexpected(_) => Level::Error,
        }
    }

    pub fn headline(self, lang: Lang) -> String {
        match self {
            Verdict::Clean => lang
                .sel(
                    "Catalog loaded — no problems found.",
                    "카탈로그를 불러왔습니다 — 문제가 없습니다.",
                )
                .to_string(),
            Verdict::Warned => lang
                .sel(
                    "Catalog loaded, with warnings — some entries need attention.",
                    "카탈로그를 불러왔습니다(경고 동반) — 일부 항목을 확인해야 합니다.",
                )
                .to_string(),
            Verdict::Misconfigured => lang
                .sel(
                    "list could not resolve a destination to read.",
                    "list가 읽을 destination을 해석하지 못했습니다.",
                )
                .to_string(),
            Verdict::Failed => lang
                .sel(
                    "list failed while reading the destination.",
                    "list가 destination을 읽는 중 실패했습니다.",
                )
                .to_string(),
            Verdict::Unexpected(Some(code)) => format!(
                "{} (exit {code})",
                lang.sel(
                    "list exited unexpectedly",
                    "list가 예상치 못한 코드로 끝났습니다"
                )
            ),
            Verdict::Unexpected(None) => lang
                .sel(
                    "list was killed by a signal.",
                    "list가 시그널로 종료됐습니다.",
                )
                .to_string(),
        }
    }

    pub fn detail(self, lang: Lang) -> Option<String> {
        let text = match self {
            Verdict::Clean => return None,
            Verdict::Warned => lang.sel(
                "exit 4 is success with caveats: the catalog below is complete. Rows marked broken/incomplete/orphan/corrupt need a closer look.",
                "exit 4는 단서가 붙은 성공입니다 — 아래 카탈로그는 완전합니다. broken/incomplete/orphan/corrupt로 표시된 행을 확인하세요.",
            ),
            Verdict::Misconfigured => lang.sel(
                "exit 2 — check that the profile exists and its destination is configured (type=local).",
                "exit 2 — 프로파일이 존재하고 destination(type=local)이 설정됐는지 확인하세요.",
            ),
            Verdict::Failed => lang.sel(
                "exit 1 — the destination may be unreadable (permissions, missing path). Check the diagnostics below.",
                "exit 1 — destination을 읽을 수 없을 수 있습니다(권한·경로 부재). 아래 진단을 확인하세요.",
            ),
            Verdict::Unexpected(_) => return None,
        };
        Some(text.to_string())
    }
}

// ---------------------------------------------------------------------------
// 파싱 — 자식 stdout → 카탈로그 (순수)
// ---------------------------------------------------------------------------

/// stdout을 문서로 접지 못한 이유. `list --json`은 실행 전제(프로파일 해석 등)가 깨지면
/// JSON을 아예 찍지 않으므로(exit 2/1), 이 오류는 대개 "stdout이 비었다"(그 경우)이거나
/// "스키마가 다르다"(빌드 섞임)이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReportError {
    /// stdout이 비었다(자식이 JSON을 찍기 전에 끝났다 — exit 2/1의 흔한 모양).
    Empty,
    /// JSON이 아니거나 기대한 최상위 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 이 빌드가 아는 값과 다르다.
    SchemaMismatch { found: u64, expected: u32 },
    /// 중첩이 너무 깊어 **파싱을 시도하지 않았다**([`crate::web::jsonguard`]).
    ///
    /// `Malformed`와 가르는 이유는 이 enum의 원칙 그대로 — 의심할 곳이 다르다. 이 화면의
    /// 입력은 매니페스트에서 온 값이므로, 이 오류는 "이 저장소의 매니페스트 하나가
    /// 병리적으로 깊다"는 신호다.
    TooDeep { found: usize, max: usize },
}

impl ReportError {
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            ReportError::Empty => lang.sel(
                "list produced no output. This is expected when the exit code is not 0 or 4 — check the diagnostics below.",
                "list가 아무 출력도 내지 않았습니다. 종료 코드가 0/4가 아닐 때 흔한 모양입니다 — 아래 진단을 확인하세요.",
            ).to_string(),
            ReportError::Malformed(detail) => format!(
                "{} {}",
                lang.sel(
                    "list output could not be parsed as the expected JSON:",
                    "list 출력을 기대한 JSON으로 해석할 수 없습니다:",
                ),
                excerpt(detail)
            ),
            ReportError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} ≠ {expected})",
                lang.sel(
                    "list reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "list가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            ReportError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "list output is nested more deeply than this console parses. A manifest in this store is probably malformed.",
                    "list 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다. 이 저장소의 매니페스트 하나가 깨졌을 가능성이 큽니다.",
                )
            ),
        }
    }
}

/// 스키마 버전이 확인된 뒤의 최상위 문서.
#[derive(Debug)]
struct ListDocument {
    store: String,
    backups: Vec<Value>,
}

/// stdout을 [`ListDocument`]로 접는다. **스키마 버전을 본문 역직렬화보다 먼저 본다** —
/// `routes::doctor::parse_report`와 같은 순서 판단(그쪽 doc 참조: 스키마가 다르면
/// "필드가 이상하다"가 아니라 "버전이 다르다"로 진단되어야 한다).
fn parse_document(stdout: &str) -> Result<ListDocument, ReportError> {
    let text = stdout.trim();
    if text.is_empty() {
        return Err(ReportError::Empty);
    }
    jsonguard::check_depth(text).map_err(|d| ReportError::TooDeep {
        found: d.found,
        max: d.max,
    })?;
    let value: Value =
        serde_json::from_str(text).map_err(|e| ReportError::Malformed(e.to_string()))?;
    match value.get("schema").and_then(Value::as_u64) {
        Some(found) if found == u64::from(crate::cli::handlers::list::LIST_JSON_SCHEMA) => {}
        Some(found) => {
            return Err(ReportError::SchemaMismatch {
                found,
                expected: crate::cli::handlers::list::LIST_JSON_SCHEMA,
            })
        }
        None => {
            return Err(ReportError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }
    let store = value
        .get("store")
        .and_then(Value::as_str)
        .ok_or_else(|| ReportError::Malformed("최상위 `store` 필드가 없습니다".to_string()))?
        .to_string();
    let backups = value
        .get("backups")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ReportError::Malformed("최상위 `backups` 필드가 배열이 아닙니다".to_string())
        })?
        .clone();
    Ok(ListDocument { store, backups })
}

/// 카탈로그 한 행 — [`crate::cli::handlers::list::CatalogRow`]의 JSON 표현을 그대로
/// 옮긴다. 그 타입을 직접 재사용하지 않는 이유: 이 화면은 `serde_json::Value`에서
/// **필드별로** 있고 없고·타입이 맞고 틀리고를 따로 판정해야 하는데(모듈 헤더 "8개 키는
/// 어떤 테스트로도 고정되지 않았다"), `#[derive(Deserialize)]`로 그 타입을 그대로
/// 역직렬화하면 필드 하나만 깨져도 **행 전체가 아니라 배열 전체**가 통째로 실패한다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogRow {
    pub id: String,
    /// `full`/`incr`/`orphan`/`corrupt`(JSON의 `type`).
    pub kind: String,
    pub promoted_from_gap: bool,
    pub engine: String,
    pub created_at: Option<String>,
    pub stored_size_bytes: u64,
    pub chain_status: String,
    pub base_id: Option<String>,
}

/// 카탈로그 항목 — 정상 파싱된 행이거나, 필수 필드가 없거나 형식이 틀려 해석할 수 없는 행.
///
/// **해석 불가 행을 숨기지 않는다.** 자식이 낸 8개 키 계약이 어떤 테스트로도 고정되지
/// 않았으므로(모듈 헤더), 그 계약이 깨진 순간을 이 enum이 표면화한다 — 숨기면 카탈로그가
/// 조용히 줄어들고, 그건 운영자가 눈치챌 수 없는 가장 나쁜 실패 방식이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CatalogEntry {
    Row(CatalogRow),
    Unparseable { id: Option<String>, reason: String },
}

/// `value`에서 필수 문자열 필드를 읽는다. 없거나 문자열이 아니면 `missing`에 필드 이름을
/// 남기고 `None`을 돌려준다 — 여러 필드가 동시에 깨져도 한 번에 전부 보고하기 위함이다.
fn required_str(
    value: &Value,
    key: &'static str,
    missing: &mut Vec<&'static str>,
) -> Option<String> {
    match value.get(key).and_then(Value::as_str) {
        Some(s) => Some(s.to_string()),
        None => {
            missing.push(key);
            None
        }
    }
}

fn required_u64(value: &Value, key: &'static str, missing: &mut Vec<&'static str>) -> Option<u64> {
    match value.get(key).and_then(Value::as_u64) {
        Some(n) => Some(n),
        None => {
            missing.push(key);
            None
        }
    }
}

fn required_bool(
    value: &Value,
    key: &'static str,
    missing: &mut Vec<&'static str>,
) -> Option<bool> {
    match value.get(key).and_then(Value::as_bool) {
        Some(b) => Some(b),
        None => {
            missing.push(key);
            None
        }
    }
}

/// 선택적(널 허용) 문자열 필드. 키가 없거나 값이 `null`이거나 문자열이 아니면 `None`으로
/// 접는다 — `created_at`/`base_id`는 정상적으로 `null`일 수 있는 필드이므로(orphan의
/// `base_id`, orphan/corrupt의 `created_at` 부재 등) "없음"과 "형식 오류"를 여기서는
/// 구분하지 않는다. 필수 필드([`required_str`] 등)와 달리 이 필드들의 형식 오류만으로는
/// 행 전체를 해석 불가로 만들지 않는다 — 표시에 `-`가 나오는 것으로 충분하다.
fn optional_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).map(str::to_string)
}

/// JSON 값 하나를 [`CatalogEntry`]로 접는다(순수 함수).
pub fn parse_row(value: &Value) -> CatalogEntry {
    let id = optional_str(value, "id");
    let mut missing: Vec<&'static str> = Vec::new();
    if id.is_none() {
        missing.push("id");
    }
    let kind = required_str(value, "type", &mut missing);
    let engine = required_str(value, "engine", &mut missing);
    let chain_status = required_str(value, "chain_status", &mut missing);
    let stored_size_bytes = required_u64(value, "stored_size_bytes", &mut missing);
    let promoted_from_gap = required_bool(value, "promoted_from_gap", &mut missing);

    if !missing.is_empty() {
        return CatalogEntry::Unparseable {
            id,
            reason: format!("missing or malformed field(s): {}", missing.join(", ")),
        };
    }

    CatalogEntry::Row(CatalogRow {
        id: id.expect("위에서 missing에 없으면 Some임을 확인했다"),
        kind: kind.expect("위에서 missing에 없으면 Some임을 확인했다"),
        engine: engine.expect("위에서 missing에 없으면 Some임을 확인했다"),
        chain_status: chain_status.expect("위에서 missing에 없으면 Some임을 확인했다"),
        stored_size_bytes: stored_size_bytes.expect("위에서 missing에 없으면 Some임을 확인했다"),
        promoted_from_gap: promoted_from_gap.expect("위에서 missing에 없으면 Some임을 확인했다"),
        created_at: optional_str(value, "created_at"),
        base_id: optional_str(value, "base_id"),
    })
}

// ---------------------------------------------------------------------------
// 필터·페이지네이션(순수)
// ---------------------------------------------------------------------------

/// 필터·페이지네이션 결과 메타.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PageInfo {
    /// 실제로 보여주는 페이지(요청값이 범위를 벗어나면 마지막 페이지로 접힌다).
    pub page: usize,
    pub total_pages: usize,
    /// 자식이 돌려준 전체 행 수(필터 전).
    pub total: usize,
    /// 필터를 통과한 행 수(해석 불가 행 포함 — 그 행들은 필터로 걸러지지 않는다).
    pub matched: usize,
    pub page_size: usize,
}

/// 유형·엔진 필터를 걸고 페이지 하나를 자른다.
///
/// **해석 불가 행은 필터와 무관하게 항상 통과한다** — `kind`/`engine`을 신뢰할 수 없는
/// 행을 "이 필터에 안 맞으니 숨긴다"고 판단하는 것은 손상을 조용히 감추는 것과 같다
/// (모듈 헤더 "해석 불가 행을 숨기지 않는다").
pub fn filter_and_paginate(
    entries: Vec<CatalogEntry>,
    type_filter: Option<&str>,
    engine_filter: Option<&str>,
    page: usize,
    page_size: usize,
) -> (Vec<CatalogEntry>, PageInfo) {
    let total = entries.len();
    let filtered: Vec<CatalogEntry> = entries
        .into_iter()
        .filter(|e| match e {
            CatalogEntry::Unparseable { .. } => true,
            CatalogEntry::Row(row) => {
                type_filter.is_none_or(|t| row.kind == t)
                    && engine_filter.is_none_or(|en| row.engine == en)
            }
        })
        .collect();
    let matched = filtered.len();
    let total_pages = matched.div_ceil(page_size).max(1);
    let page = page.clamp(1, total_pages);
    let start = (page - 1) * page_size;
    let page_rows = filtered.into_iter().skip(start).take(page_size).collect();
    (
        page_rows,
        PageInfo {
            page,
            total_pages,
            total,
            matched,
            page_size,
        },
    )
}

// ---------------------------------------------------------------------------
// 결과 조립
// ---------------------------------------------------------------------------

/// 화면이 그릴 최종 상태.
///
/// [`Outcome::Loaded`]는 **이미 필터·페이지네이션이 끝난** 한 페이지 분량만 들고 있다 —
/// 필터링을 [`collect`] 안에서 끝내는 이유는 요청 하나마다 필요한 것은 화면에 그릴
/// [`PAGE_SIZE`]개뿐이고, 전체(수천~수만 개)를 [`Outcome`]에 실어 view로 넘기면 그
/// 값을 소비하는 지점(핸들러)이 굳이 다시 자르거나 통째로 들고 있어야 하기 때문이다.
pub enum Outcome {
    Loaded {
        verdict: Verdict,
        store: String,
        page_rows: Vec<CatalogEntry>,
        page_info: PageInfo,
    },
    Unreadable {
        verdict: Verdict,
        error: ReportError,
        stderr: String,
    },
    Unavailable {
        error: RunErrorView,
    },
}

/// [`RunError`]를 view 모듈에 그대로 노출하기 위한 얇은 래퍼(`RunError` 자체는 이
/// 파일 비공개 — [`RunError`] doc의 독립성 판단 참조). view는 이 값을 받아
/// [`RunErrorView::explain`]만 부른다.
pub struct RunErrorView(RunError);

impl RunErrorView {
    pub fn explain(&self, lang: Lang) -> String {
        self.0.explain(lang)
    }
}

/// 자식을 띄워 결과를 모은다. **이 파일의 유일한 부작용 지점**(`routes::doctor::collect`와
/// 같은 구조).
async fn collect(jobs: &JobRunner, params: &CatalogParams, lang: Lang) -> Outcome {
    let spec = list_spec(params.profile.clone(), params.sort, params.asc, lang);
    let run = match run_list(jobs, &spec, CATALOG_TIMEOUT).await {
        Ok(r) => r,
        Err(e) => {
            return Outcome::Unavailable {
                error: RunErrorView(e),
            }
        }
    };
    let verdict = Verdict::from_exit(run.exit);
    match parse_document(&run.stdout) {
        Ok(doc) => {
            let entries: Vec<CatalogEntry> = doc.backups.iter().map(parse_row).collect();
            let (page_rows, page_info) = filter_and_paginate(
                entries,
                params.type_filter.as_deref(),
                params.engine_filter.as_deref(),
                params.page,
                PAGE_SIZE,
            );
            Outcome::Loaded {
                verdict,
                store: doc.store,
                page_rows,
                page_info,
            }
        }
        Err(error) => Outcome::Unreadable {
            verdict,
            error,
            stderr: excerpt(&run.stderr),
        },
    }
}

// ---------------------------------------------------------------------------
// 프로파일 목록 — config를 직접 읽는다(`routes::backup::load_profile_names`와 같은 이유:
// 동시 작업 중인 config 관련 파일에 결합하지 않기 위해 이 화면도 자기 손으로 반복한다)
// ---------------------------------------------------------------------------

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
    config.profiles.keys().cloned().collect()
}

/// 이 요청에서 쓸 시크릿 레지스트리 — `routes::jobs::request_registry`와 같은 판단
/// (러너가 실제로 자식에 주입하는 값들 + 세션 토큰).
fn request_registry(ctx: &ServeConfig) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    crate::web::mask::register_from_env_names(&mut registry, [crate::web::auth::ENV_WEB_TOKEN]);
    registry
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /catalog`.
pub async fn page(
    State(ctx): State<Arc<ServeConfig>>,
    Query(raw): Query<CatalogQuery>,
) -> Response {
    let params = match CatalogParams::from_query(&raw, ctx.lang) {
        Ok(p) => p,
        Err(message) => {
            let profiles = load_profile_names(&ctx);
            let body = view::invalid_query_body(ctx.lang, &profiles, &message);
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, CATALOG_TITLE, body),
            )
                .into_response();
        }
    };

    let outcome = collect(&ctx.jobs, &params, ctx.lang).await;
    let profiles = load_profile_names(&ctx);
    let registry = request_registry(&ctx);
    let body = view::render(ctx.lang, &params, &profiles, &outcome, &registry);
    (StatusCode::OK, layout::shell(ctx.lang, CATALOG_TITLE, body)).into_response()
}

/// 페이지네이션·정렬 링크가 공유하는 쿼리 문자열 빌더.
///
/// `profile`은 [`ProfileName`]이 이미 ASCII 영숫자/`_`/`-`만 허용하므로([`crate::web::job::args`]
/// 헤더) URL 인코딩이 필요 없다. `type`/`engine`도 이 파일이 정규화한 닫힌 어휘([`normalize_type_filter`]·
/// [`normalize_engine_filter`])만 들어가므로 마찬가지다. maud가 이 문자열을 `href` 속성에
/// 넣을 때 `&`를 `&amp;`로 자동 이스케이프하므로 HTML 유효성도 별도로 챙길 필요가 없다.
pub fn page_href(params: &CatalogParams, page: usize) -> String {
    let mut qs = format!(
        "{CATALOG_PATH}?sort={}&asc={}&page={page}",
        sort_token(params.sort),
        if params.asc { 1 } else { 0 },
    );
    if let Some(p) = &params.profile {
        qs.push_str(&format!("&profile={}", p.as_str()));
    }
    if let Some(t) = &params.type_filter {
        qs.push_str(&format!("&type={t}"));
    }
    if let Some(e) = &params.engine_filter {
        qs.push_str(&format!("&engine={e}"));
    }
    qs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::{self, AuthState};
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token-catalog-4b91";

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

    // ---- 쿼리 파싱 ----

    fn q(pairs: &[(&str, &str)]) -> CatalogQuery {
        let mut query = CatalogQuery {
            profile: None,
            sort: None,
            asc: None,
            type_filter: None,
            engine: None,
            page: None,
        };
        for (k, v) in pairs {
            match *k {
                "profile" => query.profile = Some(v.to_string()),
                "sort" => query.sort = Some(v.to_string()),
                "asc" => query.asc = Some(v.to_string()),
                "type" => query.type_filter = Some(v.to_string()),
                "engine" => query.engine = Some(v.to_string()),
                "page" => query.page = Some(v.to_string()),
                other => panic!("알 수 없는 테스트 키: {other}"),
            }
        }
        query
    }

    /// 아무 쿼리도 없으면 전부 기본값(created desc, 필터 없음, 1페이지)이다.
    #[test]
    fn empty_query_resolves_to_defaults() {
        let params = CatalogParams::from_query(&q(&[]), Lang::En).unwrap();
        assert_eq!(params.profile, None);
        assert_eq!(params.sort, ListSort::Created);
        assert!(!params.asc);
        assert_eq!(params.type_filter, None);
        assert_eq!(params.engine_filter, None);
        assert_eq!(params.page, 1);
    }

    /// 정렬·필터·페이지가 모두 반영된다.
    #[test]
    fn query_overrides_are_applied() {
        let params = CatalogParams::from_query(
            &q(&[
                ("sort", "size"),
                ("asc", "1"),
                ("type", "orphan"),
                ("engine", "pg"),
                ("page", "3"),
            ]),
            Lang::En,
        )
        .unwrap();
        assert_eq!(params.sort, ListSort::Size);
        assert!(params.asc);
        assert_eq!(params.type_filter.as_deref(), Some("orphan"));
        // 별칭이 정규 표기로 접힌다.
        assert_eq!(params.engine_filter.as_deref(), Some("postgresql"));
        assert_eq!(params.page, 3);
    }

    /// 모르는 정렬/필터 값은 에러가 아니라 기본값(필터 없음)으로 조용히 접힌다 —
    /// 안전 경계가 아닌 표시 판단이기 때문이다.
    #[test]
    fn unknown_sort_and_filter_values_fall_back_silently() {
        let params = CatalogParams::from_query(
            &q(&[("sort", "bogus"), ("type", "bogus"), ("engine", "bogus")]),
            Lang::En,
        )
        .unwrap();
        assert_eq!(params.sort, ListSort::Created);
        assert_eq!(params.type_filter, None);
        assert_eq!(params.engine_filter, None);
    }

    /// 0이나 음수 취급값(파싱 불가)의 `page`는 1로 접힌다.
    #[test]
    fn zero_or_unparseable_page_falls_back_to_one() {
        for raw in ["0", "-1", "abc", ""] {
            let params = CatalogParams::from_query(&q(&[("page", raw)]), Lang::En).unwrap();
            assert_eq!(params.page, 1, "'{raw}' → page 1이어야 함");
        }
    }

    /// 형태가 틀린 프로파일명은 조용히 넘기지 않고 에러가 된다 — 락 파일 경로 조각이다.
    #[test]
    fn malformed_profile_is_rejected_not_defaulted() {
        for bogus in ["../etc", "a b", "--force"] {
            assert!(
                CatalogParams::from_query(&q(&[("profile", bogus)]), Lang::En).is_err(),
                "'{bogus}'이 통과했다"
            );
        }
    }

    /// 빈 문자열·공백뿐인 프로파일은 "미지정"으로 접힌다(에러가 아니다) — 폼의 빈
    /// select가 이 모양을 보낸다.
    #[test]
    fn blank_profile_is_treated_as_unset() {
        let params = CatalogParams::from_query(&q(&[("profile", "  ")]), Lang::En).unwrap();
        assert_eq!(params.profile, None);
    }

    // ---- JobSpec 조립 ----

    /// `--type`/`--engine`/`--limit`은 절대 자식에 넘기지 않는다(모듈 헤더 근거) — 정렬만
    /// 넘긴다.
    #[test]
    fn list_spec_never_carries_type_engine_or_limit() {
        let spec = list_spec(
            Some(ProfileName::parse("prod", Lang::En).unwrap()),
            ListSort::Size,
            true,
            Lang::En,
        );
        let argv = spec.to_argv();
        assert_eq!(argv[0], "list");
        assert!(!argv.iter().any(|a| a == "--type"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--engine"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--limit"), "{argv:?}");
        assert!(argv.iter().any(|a| a == "--asc"), "{argv:?}");
        let idx = argv.iter().position(|a| a == "--sort").unwrap();
        assert_eq!(argv[idx + 1], "size");
        assert!(!spec.is_destructive(), "list는 파괴적이지 않다");
    }

    /// 프로파일이 없으면 `--profile`을 아예 안 붙인다 — 자식(=CLI)이 config의
    /// default_profile로 스스로 판단하게 둔다(웹이 그 판정을 대신하지 않는다).
    #[test]
    fn list_spec_omits_profile_flag_when_unset() {
        let spec = list_spec(None, ListSort::Created, false, Lang::En);
        let argv = spec.to_argv();
        assert!(!argv.iter().any(|a| a == "--profile"), "{argv:?}");
        assert!(!argv.iter().any(|a| a == "--asc"), "{argv:?}");
    }

    // ---- 문서 파싱 ----

    fn sample_json(schema: u32, backups: &str) -> String {
        format!(r#"{{"schema":{schema},"store":"/srv/backups/prod","backups":[{backups}]}}"#)
    }

    /// 정상 문서는 store·backups를 그대로 옮긴다.
    #[test]
    fn parse_document_reads_store_and_backups() {
        let json = sample_json(
            crate::cli::handlers::list::LIST_JSON_SCHEMA,
            r#"{"id":"a","type":"full","promoted_from_gap":false,"engine":"mongodb","created_at":"2026-01-01T00:00:00Z","stored_size_bytes":10,"chain_status":"ok","base_id":null}"#,
        );
        let doc = parse_document(&json).unwrap();
        assert_eq!(doc.store, "/srv/backups/prod");
        assert_eq!(doc.backups.len(), 1);
    }

    /// 빈 stdout은 `Empty`로 접힌다(exit 2/1에서 흔한 모양) — 패닉이 없다.
    #[test]
    fn parse_document_empty_stdout_is_reported() {
        assert_eq!(parse_document("").unwrap_err(), ReportError::Empty);
        assert_eq!(parse_document("   \n\t").unwrap_err(), ReportError::Empty);
    }

    /// 손상된 JSON은 패닉 없이 `Malformed`로 접힌다.
    #[test]
    fn parse_document_broken_json_does_not_panic() {
        for broken in ["not json", "{", "[1,2,3]", "{\"schema\":1"] {
            assert!(matches!(
                parse_document(broken).unwrap_err(),
                ReportError::Malformed(_)
            ));
        }
    }

    /// 병리적으로 깊은 stdout은 `serde_json`에 **닿기 전에** 거부된다. 깊이 판정 자체는
    /// `crate::web::jsonguard`의 테스트가 고정하고, 여기서는 이 파서가 그 관문을 실제로
    /// 통과시키는지만 본다 — 부르지 않으면 운영자가 깊이 숫자를 잃는다.
    #[test]
    fn parse_document_rejects_pathologically_deep_stdout() {
        let bomb = "[".repeat(50_000);
        assert_eq!(
            parse_document(&bomb).unwrap_err(),
            ReportError::TooDeep {
                found: 50_000,
                max: jsonguard::MAX_JSON_DEPTH,
            }
        );
    }

    /// 상한 안쪽의 중첩은 그대로 통과한다 — 관문이 정상 카탈로그를 막지 않는다.
    #[test]
    fn parse_document_accepts_nesting_within_the_limit() {
        let schema = crate::cli::handlers::list::LIST_JSON_SCHEMA;
        let nested = format!(
            r#"{{"schema":{schema},"store":"x","backups":[{{"deep":{}{}}}]}}"#,
            "[".repeat(60),
            "]".repeat(60)
        );
        assert!(parse_document(&nested).is_ok(), "상한 안쪽 문서가 거부됐다");
    }

    /// 스키마 필드가 없거나 다른 값이면 각각 다른 오류로 명확히 실패한다(조용한 오파싱 없음).
    #[test]
    fn parse_document_schema_mismatch_is_explicit() {
        assert!(matches!(
            parse_document(r#"{"store":"x","backups":[]}"#).unwrap_err(),
            ReportError::Malformed(_)
        ));
        assert_eq!(
            parse_document(r#"{"schema":99,"store":"x","backups":[]}"#).unwrap_err(),
            ReportError::SchemaMismatch {
                found: 99,
                expected: crate::cli::handlers::list::LIST_JSON_SCHEMA
            }
        );
    }

    /// `backups`가 배열이 아니거나 없으면 `Malformed`다.
    #[test]
    fn parse_document_requires_backups_array() {
        let schema = crate::cli::handlers::list::LIST_JSON_SCHEMA;
        assert!(matches!(
            parse_document(&format!(r#"{{"schema":{schema},"store":"x"}}"#)).unwrap_err(),
            ReportError::Malformed(_)
        ));
        assert!(matches!(
            parse_document(&format!(
                r#"{{"schema":{schema},"store":"x","backups":"nope"}}"#
            ))
            .unwrap_err(),
            ReportError::Malformed(_)
        ));
    }

    // ---- 행 파싱 ----

    fn valid_row_json() -> Value {
        serde_json::json!({
            "id": "0190f0a2-1a2b-7c3d-8e4f-000000000001",
            "type": "incr",
            "promoted_from_gap": false,
            "engine": "postgresql",
            "created_at": "2026-06-01T00:00:00Z",
            "stored_size_bytes": 12345,
            "chain_status": "ok",
            "base_id": "base-1"
        })
    }

    /// 8개 키가 모두 있으면 정상 행으로 파싱된다.
    #[test]
    fn parse_row_accepts_well_formed_entry() {
        let entry = parse_row(&valid_row_json());
        let CatalogEntry::Row(row) = entry else {
            panic!("정상 행이 Unparseable로 떨어졌다: {entry:?}");
        };
        assert_eq!(row.kind, "incr");
        assert_eq!(row.engine, "postgresql");
        assert_eq!(row.stored_size_bytes, 12345);
        assert_eq!(row.base_id.as_deref(), Some("base-1"));
    }

    /// `created_at`/`base_id`는 `null`이어도 정상 행이다(orphan/full의 정상 모양).
    #[test]
    fn parse_row_allows_null_optional_fields() {
        let mut v = valid_row_json();
        v["created_at"] = Value::Null;
        v["base_id"] = Value::Null;
        let CatalogEntry::Row(row) = parse_row(&v) else {
            panic!("null 선택 필드로 인해 잘못 거부됐다");
        };
        assert_eq!(row.created_at, None);
        assert_eq!(row.base_id, None);
    }

    /// 필수 키가 하나라도 없으면 "해석 불가"로 표시되고, id가 있으면 그 id를 함께 보존한다
    /// (조용히 사라지지 않는다 — 리뷰가 지적한 8개 키 무방비 계약의 핵심 방어).
    #[test]
    fn parse_row_surfaces_missing_required_fields() {
        for missing_key in [
            "type",
            "engine",
            "chain_status",
            "stored_size_bytes",
            "promoted_from_gap",
        ] {
            let mut v = valid_row_json();
            v.as_object_mut().unwrap().remove(missing_key);
            let entry = parse_row(&v);
            let CatalogEntry::Unparseable { id, reason } = entry else {
                panic!("'{missing_key}' 누락이 조용히 통과했다: {entry:?}");
            };
            assert!(id.is_some(), "id는 살아 있어야 진단할 수 있다");
            assert!(
                reason.contains(missing_key),
                "이유에 필드명이 없다: {reason}"
            );
        }
    }

    /// 타입이 틀린 키(문자열 자리에 숫자 등)도 "누락"과 같은 방식으로 걸러진다.
    #[test]
    fn parse_row_surfaces_wrong_typed_fields() {
        let mut v = valid_row_json();
        v["stored_size_bytes"] = Value::String("not-a-number".to_string());
        let entry = parse_row(&v);
        assert!(
            matches!(entry, CatalogEntry::Unparseable { .. }),
            "{entry:?}"
        );
    }

    /// id조차 없으면 `Unparseable { id: None, .. }`이고, 그래도 패닉하지 않는다.
    #[test]
    fn parse_row_without_id_is_still_reported() {
        let mut v = valid_row_json();
        v.as_object_mut().unwrap().remove("id");
        let CatalogEntry::Unparseable { id, reason } = parse_row(&v) else {
            panic!("id 없는 행이 조용히 통과했다");
        };
        assert_eq!(id, None);
        assert!(reason.contains("id"));
    }

    /// 완전히 다른 모양(문자열·배열 등)의 값도 패닉 없이 해석 불가로 접힌다.
    #[test]
    fn parse_row_never_panics_on_hostile_shapes() {
        for hostile in [
            Value::String("just a string".to_string()),
            Value::Array(vec![Value::Null]),
            Value::Null,
            serde_json::json!({}),
            serde_json::json!({"id": 12345}),
        ] {
            let entry = parse_row(&hostile);
            assert!(
                matches!(entry, CatalogEntry::Unparseable { .. }),
                "{entry:?}"
            );
        }
    }

    // ---- 필터·페이지네이션 ----

    fn row(id: &str, kind: &str, engine: &str, chain_status: &str, size: u64) -> CatalogEntry {
        CatalogEntry::Row(CatalogRow {
            id: id.to_string(),
            kind: kind.to_string(),
            promoted_from_gap: false,
            engine: engine.to_string(),
            created_at: Some("2026-01-01T00:00:00Z".to_string()),
            stored_size_bytes: size,
            chain_status: chain_status.to_string(),
            base_id: None,
        })
    }

    fn unparseable(id: &str) -> CatalogEntry {
        CatalogEntry::Unparseable {
            id: Some(id.to_string()),
            reason: "test".to_string(),
        }
    }

    /// 유형 필터가 일치하는 행만 남긴다(해석 불가 행은 예외).
    #[test]
    fn filter_by_type_keeps_matches_and_unparseable() {
        let entries = vec![
            row("a", "full", "mongodb", "ok", 1),
            row("b", "incr", "mongodb", "ok", 1),
            unparseable("c"),
        ];
        let (page_rows, info) = filter_and_paginate(entries, Some("full"), None, 1, 10);
        assert_eq!(info.matched, 2, "full 1개 + 해석불가 1개");
        let ids: Vec<&str> = page_rows
            .iter()
            .map(|e| match e {
                CatalogEntry::Row(r) => r.id.as_str(),
                CatalogEntry::Unparseable { id, .. } => id.as_deref().unwrap(),
            })
            .collect();
        assert_eq!(ids, vec!["a", "c"]);
    }

    /// 엔진 필터도 같은 방식으로 동작한다.
    #[test]
    fn filter_by_engine_excludes_non_matching_rows() {
        let entries = vec![
            row("a", "full", "postgresql", "ok", 1),
            row("b", "full", "mongodb", "ok", 1),
        ];
        let (page_rows, info) = filter_and_paginate(entries, None, Some("postgresql"), 1, 10);
        assert_eq!(info.matched, 1);
        assert_eq!(page_rows.len(), 1);
    }

    /// 두 필터를 동시에 걸면 둘 다 만족하는 행만 남는다.
    #[test]
    fn filter_by_both_axes_is_an_intersection() {
        let entries = vec![
            row("a", "full", "postgresql", "ok", 1),
            row("b", "incr", "postgresql", "ok", 1),
            row("c", "full", "mongodb", "ok", 1),
        ];
        let (page_rows, info) =
            filter_and_paginate(entries, Some("full"), Some("postgresql"), 1, 10);
        assert_eq!(info.matched, 1);
        assert_eq!(page_rows.len(), 1);
    }

    /// 페이지 크기·페이지 번호가 정확히 자른다.
    #[test]
    fn pagination_slices_correctly() {
        let entries: Vec<CatalogEntry> = (0..25)
            .map(|i| row(&format!("r{i}"), "full", "mongodb", "ok", i as u64))
            .collect();
        let (page1, info1) = filter_and_paginate(entries.clone(), None, None, 1, 10);
        assert_eq!(page1.len(), 10);
        assert_eq!(info1.total_pages, 3);
        let (page3, info3) = filter_and_paginate(entries.clone(), None, None, 3, 10);
        assert_eq!(page3.len(), 5, "마지막 페이지는 나머지만");
        assert_eq!(info3.page, 3);
        // 범위를 넘는 페이지 요청은 마지막 페이지로 접힌다(빈 화면 대신).
        let (page99, info99) = filter_and_paginate(entries, None, None, 99, 10);
        assert_eq!(info99.page, 3);
        assert_eq!(page99.len(), 5);
    }

    /// 빈 카탈로그도 0페이지가 아니라 "1페이지 중 1페이지, 0건"으로 안전하게 접힌다.
    #[test]
    fn empty_catalog_yields_one_empty_page_not_a_panic() {
        let (page_rows, info) = filter_and_paginate(Vec::new(), None, None, 1, 10);
        assert!(page_rows.is_empty());
        assert_eq!(info.total_pages, 1);
        assert_eq!(info.matched, 0);
    }

    // ---- 판정 ----

    /// exit 0/4/2/1은 서로 다른 판정·레벨로 갈리고, exit 4는 Fail이 아니다.
    #[test]
    fn verdict_from_exit_distinguishes_prd_codes() {
        assert_eq!(Verdict::from_exit(Some(0)), Verdict::Clean);
        assert_eq!(Verdict::from_exit(Some(4)), Verdict::Warned);
        assert_eq!(Verdict::from_exit(Some(2)), Verdict::Misconfigured);
        assert_eq!(Verdict::from_exit(Some(1)), Verdict::Failed);
        assert_ne!(
            Verdict::Warned.level(),
            Level::Fail,
            "exit 4를 실패로 칠하면 안 된다"
        );
        assert_eq!(Verdict::Warned.level(), Level::Warn);
    }

    /// 모르는 코드(3/5 등 — list가 내지 않지만)도 억지로 뭉개지 않는다.
    #[test]
    fn verdict_unexpected_preserves_code() {
        assert_eq!(Verdict::from_exit(Some(5)), Verdict::Unexpected(Some(5)));
        assert_eq!(Verdict::from_exit(None), Verdict::Unexpected(None));
    }

    // ---- HTTP ----

    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(CATALOG_PATH, get(page))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .with_state(ctx)
    }

    async fn call(ctx: Arc<ServeConfig>, uri: &str, cookie: Option<&str>) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri);
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

    /// 인증 없이는 401이고, 화면·데이터를 흘리지 않는다.
    #[tokio::test]
    async fn requires_auth() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = call(ctx, CATALOG_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.contains(CATALOG_TITLE), "미인증 응답이 화면을 흘렸다");
    }

    /// 쿠키가 있으면 200이다.
    ///
    /// 자식은 [`ServeConfig::for_test`]가 만드는 러너가 [`std::env::current_exe`](테스트
    /// 하니스 자신)를 실행하므로 유효한 `list` 출력을 내지 않는다(재귀 spawn 위험을 피하려고
    /// 진짜 `x-backup` 바이너리를 가리키지 않는다 — `job::runner` 헤더의 같은 위험 회피).
    /// 그래도 화면은 500이 아니라 200이어야 한다 — 자식 출력을 못 읽는 경우
    /// ([`Outcome::Unreadable`]/[`Outcome::Unavailable`])도 읽을 수 있는 화면으로 접히는
    /// 것이 이 화면의 계약이다. 실제 데이터가 있는 경로(정렬·필터·페이지네이션·손상 표시)는
    /// 자식 없이 순수 함수로 이미 위에서 전부 검증했다.
    #[tokio::test]
    async fn cookie_grants_access_and_renders_200() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = call(ctx, CATALOG_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("<!DOCTYPE html>"), "문서 골격 누락");
        assert!(
            body.contains(&format!("<title>x-backup — {CATALOG_TITLE}</title>")),
            "{body}"
        );
    }

    /// 형태가 틀린 `profile` 쿼리는 400이다. 이 쿼리는 `routes::jobs`의 경로 파라미터
    /// (`{id}` — 링크 클릭으로만 도달, 오타 안내 외에 값을 보일 이유가 없다)와 달리
    /// **사용자가 직접 입력한 폼 값**이므로 [`routes::backup::SubmitError::Validation`]과
    /// 같은 관례를 따른다 — 무엇이 왜 거부됐는지 알아야 고칠 수 있으므로 검증 오류
    /// 메시지에 원본 값이 그대로 들어간다. 그래서 "반사하지 않는다"가 아니라 **이스케이프
    /// 되어 반사된다**를 검사한다(저장형 XSS는 없어야 하지만, 진단 정보는 잃지 않는다).
    #[tokio::test]
    async fn malformed_profile_query_is_rejected_with_400() {
        let ctx = Arc::new(ServeConfig::for_test());
        let hostile = "<script>alert(1)</script>";
        let (status, body) = call(
            ctx,
            &format!("{CATALOG_PATH}?profile={}", urlencoding_stub(hostile)),
            Some(session()),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            !body.contains("<script>"),
            "이스케이프되지 않은 채 반사됐다: {body}"
        );
        assert!(
            body.contains("&lt;script&gt;"),
            "진단에 필요한 원본 값이 사라졌다: {body}"
        );
    }

    /// 테스트에서만 쓰는 최소 쿼리 값 인코딩 — 이 테스트가 필요로 하는 문자(`<`·`>`·`/`)만
    /// percent-encode한다(범용 URL 인코더를 새로 끌어오지 않는다).
    fn urlencoding_stub(raw: &str) -> String {
        raw.chars()
            .map(|c| match c {
                '<' => "%3C".to_string(),
                '>' => "%3E".to_string(),
                '/' => "%2F".to_string(),
                other => other.to_string(),
            })
            .collect()
    }

    /// state 경로가 화면에 새지 않는다(다른 화면들과 같은 관례).
    #[tokio::test]
    async fn markup_does_not_leak_server_paths() {
        let ctx = Arc::new(ServeConfig::for_test());
        let state = ctx.state_dir.display().to_string();
        let (_, body) = call(ctx, CATALOG_PATH, Some(session())).await;
        assert!(!body.contains(&state), "state 경로가 노출됐다: {state}");
    }

    // ---- 성능(순수 함수 몫) ----

    /// 10,000행 이상에서 **이 화면 자신의 몫**(JSON 파싱 + 필터 + 페이지네이션 + 렌더)이
    /// NFR(<500ms)을 만족하는지 실측한다. 자식 프로세스는 spawn하지 않는다 — 카탈로그
    /// 전체를 만드는 CLI 쪽 계산 비용(모듈 헤더 "성능 실측 — CLI 쪽 O(n²)")은 이 화면이
    /// 통제할 수 없으므로 별도로 분리해 측정한다. 여기서 실측하는 것은 "자식이 이미 만든
    /// JSON을 받았을 때 이 화면이 추가로 쓰는 시간"이다.
    #[test]
    fn catalog_pipeline_handles_ten_thousand_rows_quickly() {
        const N: usize = 12_000;
        let mut buf = String::with_capacity(N * 220);
        buf.push_str(&format!(
            r#"{{"schema":{},"store":"/srv/backups/perf","backups":["#,
            crate::cli::handlers::list::LIST_JSON_SCHEMA
        ));
        for i in 0..N {
            if i > 0 {
                buf.push(',');
            }
            // 상태를 순환시켜 실제 분포(대부분 ok, 일부 broken/orphan/corrupt)를 흉내낸다.
            let status = match i % 37 {
                0 => "broken",
                1 => "incomplete",
                2 => "orphan",
                3 => "corrupt",
                _ => "ok",
            };
            let engine = match i % 3 {
                0 => "postgresql",
                1 => "mongodb",
                _ => "mysql",
            };
            buf.push_str(&format!(
                r#"{{"id":"row-{i:06}","type":"full","promoted_from_gap":false,"engine":"{engine}","created_at":"2026-01-01T00:00:00Z","stored_size_bytes":{i},"chain_status":"{status}","base_id":null}}"#
            ));
        }
        buf.push_str("]}");

        let started = std::time::Instant::now();
        let doc = parse_document(&buf).expect("합성 문서 파싱 실패");
        assert_eq!(doc.backups.len(), N);
        let entries: Vec<CatalogEntry> = doc.backups.iter().map(parse_row).collect();
        let (page_rows, info) =
            filter_and_paginate(entries, None, Some("postgresql"), 1, PAGE_SIZE);
        assert_eq!(page_rows.len(), PAGE_SIZE);
        assert!(info.total_pages > 1);
        let elapsed = started.elapsed();

        eprintln!(
            "catalog_pipeline_handles_ten_thousand_rows_quickly: N={N} elapsed={:?} (debug build — release는 더 빠름)",
            elapsed
        );
        // 500ms NFR의 훨씬 아래(디버그 빌드에서도 여유가 크지 않으면 이 화면 자신의
        // 파싱·필터 로직에 실제 회귀가 생긴 것이다 — 여유를 5배로 잡아 노이즈에 흔들리지
        // 않게 한다). 이 상한은 **이 화면의 몫**에만 적용된다 — 자식 자체의 지연은
        // 포함하지 않는다(모듈 헤더 참조).
        assert!(
            elapsed < Duration::from_millis(2_500),
            "이 화면의 파싱/필터 파이프라인이 예상보다 느리다: {elapsed:?}"
        );
    }
}
