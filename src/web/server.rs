//! 라우터 조립·리스너 바인딩·graceful shutdown.
//!
//! ## 라우터를 두 덩이로 나눈 이유 — 인증이 걸리는 지점
//! [`router`]는 [`public_routes`]와 [`app_routes`]를 합친다. 이 경계는 장식이 아니라
//! **인증 계층이 걸리는 지점**이다:
//!
//! - [`public_routes`]: 토큰 없이 열려야 하는 것들. `/healthz`는 리버스 프록시·systemd·
//!   로드밸런서가 자격 증명 없이 찌른다. 정적 에셋과 로그인 화면(`/login`)도 공개여야
//!   한다 — 안 그러면 로그인 화면 자체가 스타일 없이 뜨거나, 로그인할 방법이 없어진다.
//! - [`app_routes`]: 화면과 API. `crate::web::auth::require_auth`가 여기에
//!   `.route_layer(...)`로 걸린다. `layer`가 아니라 `route_layer`여야 한다 — `layer`는
//!   404 응답에도 실행돼 존재하지 않는 경로에서까지 인증을 요구하고, 그러면 "없는 경로"와
//!   "권한 없는 경로"가 구분되지 않는다.
//!
//! 인증 상태(`Arc<auth::AuthState>`)는 라우터의 `State`(=`Arc<ServeConfig>`)와는 별개로
//! 흐른다 — `ServeConfig`에 필드를 얹지 않고, [`app_routes`]에는
//! `middleware::from_fn_with_state`로(그 미들웨어만의 독립된 state로), 로그인 핸들러에는
//! `Extension<Arc<AuthState>>`로 전달한다. `ServeConfig`는 t5가 만든 골격 구조체이고 이후
//! 태스크들도 계속 참조하므로, 인증 전용 상태를 그 정의에 끼워 넣지 않는 편이 관심사를
//! 깨끗이 가른다.
//!
//! ## graceful shutdown
//! systemd `Restart=`와 컨테이너 오케스트레이터는 SIGTERM으로 재시작을 알린다. SIGINT만
//! 처리하면 배포마다 진행 중인 응답이 끊긴다. 두 시그널 모두를 종료 신호로 받고, axum이
//! 진행 중인 요청을 마무리할 시간을 준다.
//!
//! **자식 프로세스는 이 종료를 따라 죽지 않는다.** 백업 자식은 스스로 락을 들고 계속
//! 돌아가고, 서버가 다시 뜨면 pid+시작시각+프로파일 3중 일치로 재부착한다. 그래서
//! 여기서 자식을 정리하려 들면 안 된다 — 2시간짜리 백업이 배포 때마다 죽는다. 그 재부착을
//! 실제로 수행하는 것이 [`serve`]가 포트를 잡기 전에 부르는
//! [`crate::web::reattach::run_at_startup`]이다(그 배치의 근거는 [`serve`] doc 참조).

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Extension, Router};
use maud::Markup;

use crate::cli::output::{config_source_label, field_line, style, Tone};
use crate::error::{Result, XBackupError};
use crate::web::auth::{self, AuthState};
use crate::web::state::jobs as job_state;
use crate::web::{reattach, routes, view, ServeConfig};

/// 헬스 체크 경로. 마크업(`/healthz` 링크)과 라우터가 같은 상수를 공유한다.
pub const HEALTHZ_PATH: &str = "/healthz";

/// 헬스 체크 본문 — 사람이 `curl`로 봤을 때도 의미가 있게 한 단어를 돌려준다.
const HEALTHZ_BODY: &str = "ok\n";

/// 기동 배너의 라벨 열 폭(`config:` 포함 표시 폭).
const BANNER_WIDTH: usize = 9;

/// 검증된 [`ServeConfig`]로 포트를 잡고 서버를 종료 신호까지 돌린다.
///
/// ## fail-closed 검증이 포트를 열기 전에 끝나야 하는 이유
/// 인증 설정([`AuthState::load_from_env`])과 age 개인키 권한
/// ([`auth::check_age_identity_permissions`])을 **`TcpListener::bind`보다 먼저** 확인한다.
/// `mod.rs::handle`이 `resolve_bind`/`resolve_state_dir`를 포트를 잡기 전에 다 끝내는 것과
/// 같은 원칙이다 — 잘못된 설정으로 반쯤(포트는 열렸는데 인증은 없는) 뜬 서버를 만들지
/// 않는다. 이 두 검사는 순수 설정 오류이므로 exit 2(Config) 계열로 올라간다.
///
/// bind 실패도 사용법·환경 문제(포트 점유·권한)이므로 exit 2 계열로 올린다 — 재시도해도
/// 달라지지 않는 종류의 실패이고, 운영자가 값을 고쳐야 한다.
///
/// ## 재부착 스캔은 **포트를 잡기 전에** 돈다
/// [`crate::web::reattach::run_at_startup`]을 `TcpListener::bind`보다 먼저 부른다. 근거:
///
/// 1. **스캔이 끝나기 전에 요청을 받으면 잘못된 409를 낸다.** `POST /backup`의 중복 실행 사전
///    거부는 인덱스의 `is_running` 항목을 그대로 믿는다 — 스캔이 아직 그 항목을 마감하지
///    않았으면, 고아 잡 때문에 막혔던 프로파일이 **재기동 직후에도 여전히 막힌 것처럼**
///    보인다. 그건 이 스캔이 고치려는 결함 자체다. 순서를 뒤집으면 그 창이 열린다.
/// 2. **스캔이 기동을 오래 붙잡지 않는다.** 비용은 `is_running` 항목 수 × pid 조회 한 번이고,
///    정상 환경에서는 0~수 개다. 병리적인 경우까지 대비해 스캔 자신이
///    [`crate::web::reattach::REATTACH_SCAN_BUDGET`] 안에서 스스로 멈춘다 — 즉 기동 지연의
///    상한이 코드로 묶여 있다.
/// 3. 이 파일의 다른 사전 검사들(인증 설정·age 키 권한)과 같은 자리다 — "반쯤 동작하는 서버를
///    만들지 않는다"는 규칙에서 나온 배치이고, 여기서는 "잘못된 409를 내는 서버"가 그 반쪽에
///    해당한다.
///
/// 다만 **스캔 실패는 기동을 막지 않는다**(에러를 전파하지 않는다) — 잡 이력은 캐시 성격이라
/// 그것 때문에 콘솔이 뜨지 못하면 운영자가 장애 대응 중에 터미널로 돌아가야 한다. 실패 시
/// stuck 항목이 남을 수 있다는 사실은 그 모듈이 로그로 알린다(그 파일 헤더 참조).
pub async fn serve(cfg: ServeConfig) -> Result<()> {
    let auth_state = Arc::new(AuthState::load_from_env()?);
    auth::check_age_identity_permissions()?;

    // 쓰기 공유 인스턴스를 쓴다 — 스캔은 종료 기록을 append하는 **쓰기** 경로다
    // (`state::jobs` 헤더 "쓰기 경로는 인스턴스 하나를 공유해야 한다").
    reattach::run_at_startup(&job_state::shared(&cfg.state_dir), cfg.lang).await;

    let listener = tokio::net::TcpListener::bind(cfg.bind).await.map_err(|e| {
        XBackupError::Config(format!(
            "{}에 바인딩할 수 없습니다: {e} — 포트가 이미 쓰이고 있거나 권한이 없습니다. \
             `--bind`로 다른 포트를 지정하세요.",
            cfg.bind
        ))
    })?;
    // 커널이 실제로 배정한 주소를 다시 읽는다 — 요청값과 다를 수 있는 경로(듀얼스택 등)에서
    // 배너가 거짓말하지 않게 한다.
    let bound: SocketAddr = listener.local_addr().map_err(XBackupError::Io)?;

    print_banner(&cfg, bound);

    let app = router(Arc::new(cfg), auth_state);
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|e| XBackupError::Failure(format!("웹 서버가 비정상 종료했습니다: {e}")))
}

/// 전체 라우터를 조립한다. 인증 계층은 [`app_routes`] 안에 붙는다(모듈 헤더 참조).
pub fn router(ctx: Arc<ServeConfig>, auth: Arc<AuthState>) -> Router {
    Router::new()
        .merge(public_routes())
        .merge(app_routes(auth.clone()))
        // 로그인 핸들러(`auth::login_form`/`login_submit`)가 `Extension<Arc<AuthState>>`로
        // 뽑아 쓴다. `app_routes`에 건 `route_layer`(from_fn_with_state)는 자신만의 독립된
        // state를 캡처하므로 이 Extension과 무관하게 동작한다 — 두 메커니즘이 각자의
        // 몫(핸들러 값 주입 vs 미들웨어 강제)을 맡는다(모듈 헤더 참조).
        .layer(Extension(auth))
        .with_state(ctx)
}

/// 인증 없이 열리는 라우트 — 헬스 체크·정적 에셋·로그인 화면.
fn public_routes() -> Router<Arc<ServeConfig>> {
    Router::new()
        .route(HEALTHZ_PATH, get(healthz))
        .route(view::APP_CSS_PATH, get(app_css))
        .route(
            auth::LOGIN_PATH,
            get(auth::login_form).post(auth::login_submit),
        )
}

/// 인증 뒤에 놓일 라우트 — 화면과 API.
///
/// `/doctor`가 여기(공개 라우트가 아니라) 있는 이유: `doctor`는 파괴적이지 않지만 **config
/// 구조를 그대로 보여준다** — 프로파일 이름·백업 저장 위치·암호화 설정은 침입자에게 정확히
/// 무엇을 어디서 훔칠 수 있는지 알려주는 지도다. 읽기 전용이라는 것이 공개해도 된다는 뜻은
/// 아니다.
fn app_routes(auth: Arc<AuthState>) -> Router<Arc<ServeConfig>> {
    Router::new()
        .route("/", get(index))
        .route(routes::doctor::DOCTOR_PATH, get(routes::doctor::page))
        // 락 현황(t13) — 읽기 전용. 부작용이 없지만 pid·hostname·프로파일이 드러나므로
        // 공개 라우트가 아니라 인증 뒤에 둔다.
        .route(routes::lock::LOCK_PATH, get(routes::lock::page))
        // 잡 이력(t14) — 목록과 상세. 상세는 경로 파라미터가 곧 파일명이 될 자리라
        // 핸들러가 id를 검증한다(`routes::jobs` 헤더 참조).
        .route(routes::jobs::JOBS_PATH, get(routes::jobs::page))
        .route(routes::jobs::JOB_DETAIL_ROUTE, get(routes::jobs::detail))
        // config 편집(t26) — 읽기·폼·검증까지다. 실제 파일 쓰기는 t27이 저장소 이음매
        // (`ChangeReceipt::persisted`)에 붙인다. 지금 `save`/`delete`는 검증된 변경을
        // 만들되 디스크를 바꾸지 않는다(`default_store_does_not_persist` 테스트로 고정).
        .route(routes::config::CONFIG_PATH, get(routes::config::list))
        .route(routes::config::NEW_PATH, get(routes::config::new_form))
        .route(routes::config::EDIT_ROUTE, get(routes::config::edit_form))
        .route(
            routes::config::DELETE_CONFIRM_ROUTE,
            get(routes::config::delete_form),
        )
        // 백업 실행(t15) — 잡 실행 경로가 처음으로 관통하는 화면.
        // `/backup/{job_id}`(정적 `/backup`과 같은 접두)는 axum이 정적 우선으로 가른다.
        // SSE(`/events`)는 장수 응답이라 앞단 리버스 프록시의 버퍼링·타임아웃 설정이
        // 필요하다 — 운영 문서(t33)에 넣을 항목이다.
        .route(
            routes::backup::BACKUP_PATH,
            get(routes::backup::form).post(routes::backup::submit),
        )
        .route(
            routes::backup::BACKUP_RUNNING_ROUTE,
            get(routes::backup::running),
        )
        .route(
            routes::backup::BACKUP_EVENTS_ROUTE,
            get(routes::backup::events),
        )
        .route(
            routes::backup::BACKUP_CANCEL_ROUTE,
            post(routes::backup::cancel),
        )
        // 대시보드(t16) — 전 프로파일 상태를 한 화면에. 자식을 여러 개 띄우므로
        // `cache::probe_cache()`의 TTL 뒤에서 돈다(핸들러 doc 참조).
        .route(
            routes::dashboard::DASHBOARD_PATH,
            get(routes::dashboard::page),
        )
        // 라이브 모니터(t18) — 화면 수와 무관하게 `status --watch --json` 자식 **하나**를
        // 공유한다. 자식을 띄우는 것은 페이지가 아니라 SSE 쪽이다(뷰어가 실제로 붙어야
        // 자식이 뜬다). 그래서 `/monitor`만 열어 두고 탭을 닫으면 자식은 뜨지 않는다.
        .route(routes::monitor::MONITOR_PATH, get(routes::monitor::page))
        .route(
            routes::monitor::MONITOR_EVENTS_PATH,
            get(routes::monitor::events),
        )
        // 카탈로그(t17) — 정렬·필터·페이지네이션. 쿼리 파라미터만 받는 읽기 화면이다.
        .route(routes::catalog::CATALOG_PATH, get(routes::catalog::page))
        // peek(t20) — 기본 마스킹. 원문 노출(`/peek/reveal`)은 POST이고 감사 로그
        // 기록이 성공해야만 렌더한다(`routes::peek` 헤더 참조).
        .route(routes::peek::PEEK_PATH, get(routes::peek::page))
        .route(routes::peek::PEEK_REVEAL_PATH, post(routes::peek::reveal))
        // verify(t22) — 제출은 잡을 띄우고, 결과는 잡 id로 다시 찾아온다.
        .route(
            routes::verify::VERIFY_PATH,
            get(routes::verify::form).post(routes::verify::submit),
        )
        .route(
            routes::verify::VERIFY_RESULT_ROUTE,
            get(routes::verify::result),
        )
        // 스케줄(t29) — CRUD. 정적 경로(`/schedule/new`)와 동적 경로
        // (`/schedule/edit/{id}`)가 접두를 공유하지만 axum이 정적 우선으로 가른다.
        .route(routes::schedule::SCHEDULE_PATH, get(routes::schedule::list))
        .route(routes::schedule::NEW_PATH, get(routes::schedule::new_form))
        .route(
            routes::schedule::EDIT_ROUTE,
            get(routes::schedule::edit_form),
        )
        .route(
            routes::schedule::DELETE_CONFIRM_ROUTE,
            get(routes::schedule::delete_form),
        )
        .route(routes::schedule::SAVE_PATH, post(routes::schedule::save))
        .route(
            routes::schedule::DELETE_PATH,
            post(routes::schedule::delete_submit),
        )
        .route(routes::config::SAVE_PATH, post(routes::config::save))
        .route(
            routes::config::DELETE_PATH,
            post(routes::config::delete_submit),
        )
        // prune(t23) — 이 콘솔에서 처음으로 파괴적 작업을 실행하는 화면이다. 폼 → 계획
        // → 실행 세 걸음이고, 실행 경로는 확인 토큰 없이는 통과하지 못한다
        // (`routes::prune` 헤더). 결과는 잡 이력 화면으로 넘긴다.
        .route(routes::prune::PRUNE_PATH, get(routes::prune::form))
        .route(routes::prune::PRUNE_PLAN_PATH, post(routes::prune::plan))
        .route(routes::prune::PRUNE_APPLY_PATH, post(routes::prune::apply))
        .route(
            routes::prune::PRUNE_RESULT_ROUTE,
            get(routes::prune::result),
        )
        // migrate(t24) — prune과 같은 세 걸음이지만 대상이 **살아 있는 서버**다. 확인
        // 화면이 요구하는 이름은 source가 아니라 target 프로파일이다(`routes::migrate` 헤더).
        .route(routes::migrate::MIGRATE_PATH, get(routes::migrate::form))
        .route(
            routes::migrate::MIGRATE_PLAN_PATH,
            post(routes::migrate::plan),
        )
        .route(
            routes::migrate::MIGRATE_APPLY_PATH,
            post(routes::migrate::apply),
        )
        .route(
            routes::migrate::MIGRATE_RESULT_ROUTE,
            get(routes::migrate::result),
        )
        // restore(t25) — 이 콘솔에서 가장 위험한 화면이다. 기본이 in-place(백업을 떠온
        // 프로덕션 서버에 되쓰기)라, 계획 화면이 대상과 그 출처를 첫 줄에 놓는다
        // (`routes::restore` 헤더).
        .route(routes::restore::RESTORE_PATH, get(routes::restore::form))
        .route(
            routes::restore::RESTORE_PLAN_PATH,
            post(routes::restore::plan),
        )
        .route(
            routes::restore::RESTORE_APPLY_PATH,
            post(routes::restore::apply),
        )
        .route(
            routes::restore::RESTORE_RESULT_ROUTE,
            get(routes::restore::result),
        )
        // v1 → v2 전환(t28) — 저장의 부작용이 아니라 **명시적 동작**이라 별도 화면이다.
        // 미리보기(GET)와 확인(POST)이 파일 지문을 공유해 그 사이의 변경을 잡는다
        // (`convert_submit` doc의 TOCTOU 절 참조).
        .route(
            routes::config::CONVERT_PATH,
            get(routes::config::convert_form).post(routes::config::convert_submit),
        )
        // 상태 변경 요청(POST 등)은 출처가 **포트까지** 일치해야 한다.
        //
        // 이 계층이 필요한 이유는 `SameSite=Strict`가 못 보는 축이 하나 있기 때문이다 —
        // 브라우저 쿠키 스코프와 same-site 판정은 **포트를 구분하지 않는다**(RFC 6265 §8.5).
        // 그래서 같은 호스트명의 다른 포트에서 도는 서비스(`:3000`의 사내 앱)가 이 콘솔의
        // 세션 쿠키를 함께 받고, 그 출처에서 온 위조 POST는 `SameSite`를 통과한다.
        // `Origin`은 정의상 스킴+호스트+**포트**이므로(RFC 6454) 그 축을 볼 수 있다.
        // 자세한 근거는 `auth::verify_same_origin` doc 참조.
        //
        // GET/HEAD/OPTIONS는 미들웨어 안에서 면제되므로 조회 화면에는 영향이 없다.
        //
        // `POST /login`은 `public_routes`에 있어 이 계층 밖이다 — 의도적이다. 로그인 CSRF는
        // "피해자를 공격자 계정으로 로그인시키는" 공격이고, 이 콘솔은 공유 토큰 하나를 쓰므로
        // 공격자가 그 토큰을 이미 알아야 성립한다. 알고 있다면 CSRF를 할 이유가 없다.
        .route_layer(middleware::from_fn(auth::require_same_origin))
        .route_layer(middleware::from_fn_with_state(auth, auth::require_auth))
}

/// `GET /healthz` — 프로세스가 요청을 받을 수 있는지만 답한다.
///
/// DB나 스토리지를 찌르지 않는다. 헬스 체크가 프로덕션 DB에 의존하면, DB 점검 때마다
/// 오케스트레이터가 멀쩡한 콘솔을 재시작한다.
async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, HEALTHZ_BODY)
}

/// `GET /assets/app.css` — 임베드된 스타일시트.
///
/// URL에 내용 지문이 붙으므로 영구 캐시가 안전하다([`view`] 헤더 참조).
async fn app_css() -> impl IntoResponse {
    (
        [
            (header::CONTENT_TYPE, "text/css; charset=utf-8"),
            (header::CACHE_CONTROL, view::ASSET_CACHE_CONTROL),
        ],
        view::APP_CSS,
    )
}

/// `GET /` — 진입 화면. 대시보드(t15)가 이 자리를 차지한다.
async fn index(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    view::layout::shell(ctx.lang, "Console", view::layout::landing(ctx.lang))
}

/// 기동 배너 — 어디에 떴고 무엇을 보고 있는지 stdout 한 덩이로 알린다.
///
/// `tracing`이 아니라 `println!`인 이유: 기본 로그 필터가 `warn`이라 `-v` 없이는 아무것도
/// 보이지 않는데, "어느 주소에서 듣는지"는 `serve`의 **결과**이므로 항상 보여야 한다.
fn print_banner(cfg: &ServeConfig, bound: SocketAddr) {
    println!(
        "{}",
        style(
            cfg.lang
                .sel("x-backup web console started", "x-backup 웹 콘솔 시작"),
            Tone::Success
        )
    );
    println!(
        "{}",
        field_line("listen", format!("http://{bound}"), BANNER_WIDTH)
    );
    println!(
        "{}",
        field_line("state", cfg.state_dir.display().to_string(), BANNER_WIDTH)
    );
    println!(
        "{}",
        field_line(
            "config",
            config_source_label(cfg.config_path.as_deref(), cfg.lang),
            BANNER_WIDTH
        )
    );
    // TLS는 이 바이너리가 종단하지 않는다(PRD Q2) — 루프백 밖으로 열었다면 앞단에 프록시가
    // 있어야 한다는 사실을 기동 시점에 상기시킨다.
    if !bound.ip().is_loopback() {
        eprintln!(
            "{}",
            crate::cli::output::style_stderr(
                cfg.lang.sel(
                    "! bound outside loopback — terminate TLS and restrict access at a reverse proxy.",
                    "! 루프백 밖에 바인딩됨 — TLS 종단과 접근 제한을 앞단 리버스 프록시에서 처리하세요.",
                ),
                Tone::Warning
            )
        );
    }
    warn_unmaskable_secrets(cfg);
}

/// 마스킹 대상으로 등록되지 못한 시크릿 env가 있으면 기동 시점에 한 번 경고한다.
///
/// ## 왜 배너에서 한 번 더 말하는가
/// [`crate::web::job::JobSecrets::insert_value`]가 이미 등록 실패 시점에 `tracing::warn!`을
/// 남긴다(기본 로그 필터가 `warn`이라 `-v` 없이도 보인다). 그런데 그 경고는 시크릿을 모으는
/// 도중에 나오므로 배너보다 **먼저** 찍히고, 운영자가 실제로 읽는 것은 "서버가 떴다"는 배너
/// 근처다. 그래서 여기서 목록을 한 줄로 요약해 배너 끝에 붙인다 — 루프백 밖 바인딩 경고와
/// 같은 자리, 같은 형식(`!` 접두 + `Tone::Warning` + stderr)이다.
///
/// **잡마다 반복하지 않는다.** 이 경고는 기동 경로에서 한 번만 나온다 — 매 잡마다 같은
/// 문장을 찍으면 진짜 신호가 소음에 묻힌다.
///
/// env **이름만** 싣는다(값은 절대). 이름은 config에 평문으로 있으므로 무해하다.
fn warn_unmaskable_secrets(cfg: &ServeConfig) {
    let names = cfg.jobs.unmaskable_env_names();
    if names.is_empty() {
        return;
    }
    let joined = names.join(", ");
    eprintln!(
        "{}",
        crate::cli::output::style_stderr(
            &format!(
                "{} {joined}",
                cfg.lang.sel(
                    "! these secret env values are too short to mask — they are injected into job \
                     children but will appear verbatim in job logs, SSE, and job history:",
                    "! 다음 시크릿 env의 값이 너무 짧아 마스킹할 수 없습니다 — 자식 프로세스에는 \
                     주입되지만 잡 로그·SSE·잡 이력에 원문이 그대로 남습니다:",
                )
            ),
            Tone::Warning
        )
    );
}

/// SIGINT(Ctrl-C) 또는 SIGTERM 중 먼저 오는 것을 기다린다.
async fn shutdown_signal() {
    let ctrl_c = async {
        // 핸들러 설치 실패는 시그널을 못 받는다는 뜻이므로, 조용히 영구 대기로 떨어뜨린다
        // (여기서 즉시 종료하면 서버가 켜지자마자 내려간다).
        match tokio::signal::ctrl_c().await {
            Ok(()) => {}
            Err(e) => {
                tracing::warn!(error = %e, "SIGINT 핸들러를 설치할 수 없습니다");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(error = %e, "SIGTERM 핸들러를 설치할 수 없습니다");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("종료 신호 수신 — 진행 중인 요청을 마무리합니다");
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// 테스트에서 쓰는 고정 토큰 — `AuthState::for_test`가 env를 거치지 않고 만든다.
    const TEST_TOKEN: &str = "test-token-abc123";

    /// 테스트용 컨텍스트. 실제 바인딩은 하지 않으므로 주소는 형식만 유효하면 된다.
    ///
    /// [`ServeConfig::for_test`]를 쓴다 — 감사 로그·잡 러너가 `ServeConfig`에 합류한 뒤로는
    /// 구조체 리터럴을 여기서 다시 조립하면 필드가 늘어날 때마다 이 파일이 깨진다.
    /// `state_dir`은 그 생성자가 잡는 임시 경로이므로, 경로 비노출을 검증하는 테스트는
    /// 리터럴 대신 컨텍스트에서 읽은 값으로 단정한다.
    fn ctx() -> Arc<ServeConfig> {
        Arc::new(ServeConfig::for_test())
    }

    /// 테스트용 인증 상태와 **그 상태에서 유효한 세션 ID**.
    ///
    /// 프로세스 전역으로 한 번만 만든다. 호출마다 새로 만들면 세션이 발급된 인스턴스와
    /// 라우터가 보는 인스턴스가 달라져 인증이 조용히 401이 된다 — 세션이 `AuthState`
    /// 안에서만 살기 때문이다(`AuthState::for_test_with_session` doc).
    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    /// 테스트용 인증 상태 — 모든 테스트가 같은 [`TEST_TOKEN`]을 기대한다.
    fn test_auth() -> Arc<AuthState> {
        Arc::clone(&auth_and_session().0)
    }

    /// 유효한 세션 쿠키 값. **[`TEST_TOKEN`]이 아니다** — 그것이 이 설계의 요점이다.
    fn test_session() -> &'static str {
        &auth_and_session().1
    }

    /// 라우터를 포트 없이 직접 호출한다(쿠키 없음 = 미인증 요청) — 실제 리스너를 띄우지
    /// 않으므로 테스트가 포트 점유·방화벽·타이밍에 흔들리지 않는다.
    async fn call(uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        call_with_cookie(uri, None).await
    }

    /// 유효한 세션 쿠키를 실어 호출한다 — 인증 뒤 라우트를 검증할 때 쓴다.
    async fn call_authed(uri: &str) -> (StatusCode, axum::http::HeaderMap, String) {
        call_with_cookie(uri, Some(test_session())).await
    }

    /// 지정한 컨텍스트로 인증된 요청을 보내고 본문만 돌려준다.
    ///
    /// 응답에 서버 내부 경로가 새는지 보는 테스트는 **자기 컨텍스트의 `state_dir` 값을
    /// 알아야** 단정할 수 있는데, [`ctx`]는 호출마다 다른 임시 디렉터리를 잡으므로
    /// `call_authed`로는 그 값을 알 수 없다. 그래서 컨텍스트를 밖에서 넘길 통로를 둔다.
    async fn call_authed_with(ctx: Arc<ServeConfig>, uri: &str) -> String {
        call_inner(ctx, uri, Some(test_session())).await.2
    }

    /// `call`/`call_authed`의 공통 본체. `cookie_value`가 `Some`이면 세션 쿠키를 헤더에 싣는다.
    async fn call_with_cookie(
        uri: &str,
        cookie_value: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        call_inner(ctx(), uri, cookie_value).await
    }

    /// 라우터를 실제로 한 번 호출한다(상태·헤더·본문 수집).
    async fn call_inner(
        ctx: Arc<ServeConfig>,
        uri: &str,
        cookie_value: Option<&str>,
    ) -> (StatusCode, axum::http::HeaderMap, String) {
        let mut builder = Request::builder().uri(uri);
        if let Some(session_id) = cookie_value {
            builder = builder.header(
                header::COOKIE,
                format!("{}={session_id}", auth::SESSION_COOKIE_NAME),
            );
        }
        let response = router(ctx, test_auth())
            .oneshot(builder.body(Body::empty()).unwrap())
            .await
            .expect("라우터 호출 실패");
        let status = response.status();
        let headers = response.headers().clone();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            headers,
            String::from_utf8_lossy(&bytes).into_owned(),
        )
    }

    /// `GET /healthz`는 인증 없이 200 + 짧은 본문. DB·스토리지에 손대지 않으므로 컨텍스트가
    /// 존재하지 않는 경로를 가리켜도 성공해야 한다.
    #[tokio::test]
    async fn healthz_returns_200() {
        let (status, _, body) = call(HEALTHZ_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, HEALTHZ_BODY);
    }

    /// 스타일시트는 인증 없이, 임베드된 내용 그대로, CSS content-type과 영구 캐시 헤더로 나간다.
    #[tokio::test]
    async fn css_route_serves_embedded_asset() {
        let (status, headers, body) = call(view::APP_CSS_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers[header::CONTENT_TYPE], "text/css; charset=utf-8");
        assert_eq!(headers[header::CACHE_CONTROL], view::ASSET_CACHE_CONTROL);
        assert_eq!(body, view::APP_CSS, "임베드 내용과 응답 본문이 달라졌다");
    }

    /// 지문 쿼리가 붙은 URL도 같은 라우트로 들어온다(쿼리는 경로 매칭에 영향 없음).
    #[tokio::test]
    async fn css_route_matches_fingerprinted_url() {
        let (status, _, _) = call(&view::app_css_url()).await;
        assert_eq!(status, StatusCode::OK);
    }

    /// 세션 쿠키 없이 `/`를 찌르면 401 — 인증 뒤 라우트가 조용히 열려 있지 않다.
    #[tokio::test]
    async fn index_without_cookie_is_401() {
        let (status, _, body) = call("/").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            !body.contains(TEST_TOKEN),
            "실패 응답에 토큰이 노출되면 안 됨"
        );
    }

    /// 틀린 쿠키 값도 401 — 쿠키가 "있기만" 하면 통과하는 사고를 막는다.
    #[tokio::test]
    async fn index_with_wrong_cookie_is_401() {
        let (status, _, _) = call_with_cookie("/", Some("not-the-token")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// `/healthz`·`/assets/app.css`는 200, `/`는 401 — 태스크가 요구하는 최소 조합을
    /// 한 테스트에 모아 회귀를 한눈에 잡는다(개별 라우트는 위에서 이미 각각 검증).
    #[tokio::test]
    async fn public_routes_open_app_routes_closed() {
        assert_eq!(call(HEALTHZ_PATH).await.0, StatusCode::OK);
        assert_eq!(call(view::APP_CSS_PATH).await.0, StatusCode::OK);
        assert_eq!(call("/").await.0, StatusCode::UNAUTHORIZED);
    }

    /// 유효한 세션 쿠키가 있으면 진입 화면은 레이아웃 껍데기를 렌더하고 스타일시트를 링크한다.
    #[tokio::test]
    async fn index_renders_shell_when_authenticated() {
        let (status, headers, body) = call_authed("/").await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            headers[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/html"),
            "HTML content-type이 아니다: {:?}",
            headers[header::CONTENT_TYPE]
        );
        assert!(body.starts_with("<!DOCTYPE html>"), "문서 골격 누락");
        assert!(body.contains(&view::app_css_url()), "스타일시트 링크 누락");
        assert!(body.contains(HEALTHZ_PATH), "헬스 체크 안내 누락");
    }

    /// `GET /doctor`는 쿠키 없이는 401 — config 구조를 보여주는 화면이 열려 있지 않다.
    #[tokio::test]
    async fn doctor_without_cookie_is_401() {
        let (status, _, body) = call(routes::doctor::DOCTOR_PATH).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(
            !body.contains("Doctor"),
            "미인증 응답이 화면 내용을 흘리면 안 됨: {body}"
        );
    }

    /// 유효한 세션 쿠키가 있으면 `GET /doctor`가 200 + 껍데기를 렌더한다.
    ///
    /// ## 이 테스트에서 실제로 무엇이 실행되는가
    /// 핸들러는 [`std::env::current_exe`]로 자기 자신을 부른다 — 단위 테스트에서 그건
    /// **테스트 바이너리**다. libtest는 `--config`/`--json`을 모르는 옵션으로 즉시 거부하고
    /// exit 101로 끝나므로(테스트가 재귀 실행되지 않는다), 이 테스트는
    /// `Verdict::Unexpected(101)` 경로 — 즉 "자식이 이상하게 끝났을 때도 읽을 수 있는 오류
    /// 화면이 200으로 나간다"를 검증한다. 실제 보고서 렌더는
    /// `routes::doctor`의 순수 함수 테스트가 실제 `doctor --json` 표본으로 다룬다.
    #[tokio::test]
    async fn doctor_with_cookie_renders_page() {
        let (status, headers, body) = call_authed(routes::doctor::DOCTOR_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert!(headers[header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .starts_with("text/html"));
        assert!(body.starts_with("<!DOCTYPE html>"), "문서 골격 누락");
        assert!(
            body.contains("<title>x-backup — Doctor</title>"),
            "화면 제목 누락: {body}"
        );
        // 자식이 어떻게 끝났든 사람이 읽을 수 있는 판정 배너가 있어야 한다.
        assert!(
            body.contains(r#"class="verdict""#),
            "판정 배너 누락: {body}"
        );
        assert!(body.contains(&view::app_css_url()), "스타일시트 링크 누락");
    }

    /// doctor 화면도 서버 내부 경로(state 디렉터리)를 흘리지 않는다.
    #[tokio::test]
    async fn doctor_markup_does_not_leak_server_paths() {
        let ctx = ctx();
        let state = ctx.state_dir.display().to_string();
        let body = call_authed_with(ctx, routes::doctor::DOCTOR_PATH).await;
        assert!(
            !body.contains(&state),
            "state 경로가 HTML로 노출됐다: {state}"
        );
    }

    /// `GET /login`은 인증 없이 폼을 보여준다.
    #[tokio::test]
    async fn login_form_is_public_and_renders_form() {
        let (status, _, body) = call(auth::LOGIN_PATH).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("action=\"/login\""), "로그인 폼 누락: {body}");
    }

    /// 올바른 토큰으로 `POST /login`하면 303 + 세션 쿠키를 받고, 그 쿠키로 `/`에 들어갈 수 있다.
    #[tokio::test]
    async fn login_submit_with_correct_token_sets_cookie_and_grants_access() {
        let body = format!("token={TEST_TOKEN}");
        let response = router(ctx(), test_auth())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(auth::LOGIN_PATH)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .expect("라우터 호출 실패");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE]
            .to_str()
            .unwrap()
            .to_string();
        assert!(
            set_cookie.contains("HttpOnly"),
            "HttpOnly 누락: {set_cookie}"
        );
        assert!(
            set_cookie.contains("SameSite=Strict"),
            "SameSite=Strict 누락: {set_cookie}"
        );
        // Secure 판정은 **바인딩이 아니라 요청 스킴**이다(`auth` 모듈 헤더 "쿠키 속성 근거").
        // 이 요청에는 `X-Forwarded-Proto`가 없으므로 평문으로 보고 Secure를 붙이지 않는다 —
        // 붙이면 브라우저가 쿠키를 버려 로그인이 303 → / → 401로 무한 반복된다.
        //
        // 예전 판정은 `!ctx.bind.ip().is_loopback()`이었고, 이 자리의 주석이 그것을
        // "루프백에는 Secure를 강제하지 않는다"로 정당화했다. 그 규칙은 **권장 배포에서
        // 정확히 틀린다** — `프록시(443) → 127.0.0.1:8787`에서 바인딩은 루프백인 채로
        // 브라우저는 https로 오므로, Secure가 꺼진 쿠키(=마스터 토큰)가 같은 호스트로
        // 향하는 평문 http 요청 하나에 실려 나간다. 그 회귀는 아래
        // `login_over_forwarded_https_marks_the_cookie_secure`가 막는다.
        assert!(
            !set_cookie.contains("Secure"),
            "평문 요청에 Secure를 붙이면 브라우저가 쿠키를 버린다: {set_cookie}"
        );

        let session_id = set_cookie
            .split(';')
            .next()
            .unwrap()
            .trim_start_matches(&format!("{}=", auth::SESSION_COOKIE_NAME));

        // **쿠키 값이 마스터 토큰이면 안 된다.** 예전에는 그랬고, 브라우저 쿠키가 포트로
        // 격리되지 않는 탓에(RFC 6265 §8.5) 같은 호스트의 다른 포트 서비스가 그 값을 함께
        // 받았다 — 즉 쿠키 한 번 유출이 곧 마스터 자격 유출이었다. 지금은 회수 가능한
        // 세션 ID만 나간다(`auth::the_master_token_is_not_accepted_as_a_session_cookie`가
        // 반대편, 즉 토큰을 넣어도 통과하지 않는다는 것을 고정한다).
        assert_ne!(
            session_id, TEST_TOKEN,
            "세션 쿠키에 마스터 토큰이 그대로 실렸다: {set_cookie}"
        );
        assert!(
            !set_cookie.contains(TEST_TOKEN),
            "Set-Cookie 어디에도 토큰이 있으면 안 된다: {set_cookie}"
        );

        let (status, _, _) = call_with_cookie("/", Some(session_id)).await;
        assert_eq!(
            status,
            StatusCode::OK,
            "발급된 쿠키로 인증 뒤 라우트에 들어가야 함"
        );

        // 반대 방향도 못박는다 — 토큰을 쿠키에 넣으면 들어갈 수 없다.
        let (status, _, _) = call_with_cookie("/", Some(TEST_TOKEN)).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "마스터 토큰을 쿠키로 보냈는데 통과했다"
        );
    }

    /// 프록시 뒤(https)에서는 세션 쿠키에 `Secure`가 붙는다.
    ///
    /// ## 이 테스트가 막는 회귀
    /// 권장 배치는 `프록시(443) → 127.0.0.1:8787`이라 **바인딩은 루프백인 채로** https
    /// 요청이 들어온다. 예전 판정(`!ctx.bind.ip().is_loopback()`)은 정확히 이 배치에서
    /// Secure를 껐다 — 브라우저는 https로 오는데 쿠키에 Secure가 없으니, 같은 호스트로
    /// 향하는 평문 http 요청 하나에 **세션 쿠키가 그대로 실려 나간다.** 바인딩 주소는 그
    /// 판단에 쓸 신호가 아니었다.
    ///
    /// (이 회귀는 쿠키 값이 마스터 토큰이던 시절에 발견됐고 그때는 곧 토큰 유출이었다.
    /// 지금은 새는 것이 만료·회수 가능한 세션 ID라 무게가 줄었지만, 평문에 실리면 안
    /// 된다는 결론은 같다 — `auth` 모듈 헤더 "쿠키 값은 토큰이 아니다".)
    #[tokio::test]
    async fn login_over_forwarded_https_marks_the_cookie_secure() {
        let response = router(ctx(), test_auth())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(auth::LOGIN_PATH)
                    .header("x-forwarded-proto", "https")
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from(format!("token={TEST_TOKEN}")))
                    .unwrap(),
            )
            .await
            .expect("라우터 호출 실패");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let set_cookie = response.headers()[header::SET_COOKIE].to_str().unwrap();
        assert!(
            set_cookie.contains("; Secure"),
            "https 요청인데 Secure가 없다 — 평문 http 요청 하나에 토큰이 실려 나간다: {set_cookie}"
        );
    }

    /// 틀린 토큰으로 `POST /login`하면 쿠키 없이 로그인 화면(`?error=1`)으로 303 리다이렉트한다.
    #[tokio::test]
    async fn login_submit_with_wrong_token_redirects_without_cookie() {
        let response = router(ctx(), test_auth())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(auth::LOGIN_PATH)
                    .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
                    .body(Body::from("token=wrong"))
                    .unwrap(),
            )
            .await
            .expect("라우터 호출 실패");
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert!(!response.headers().contains_key(header::SET_COOKIE));
        assert_eq!(
            response.headers()[header::LOCATION],
            "/login?error=1",
            "실패는 에러 배너가 붙은 로그인 화면으로 돌아가야 함"
        );
    }

    /// 없는 경로는 404 — 아직 붙지 않은 화면이 조용히 200을 돌려주지 않는다. 인증 미들웨어는
    /// `route_layer`로 붙어 있어 매칭되지 않은 경로에는 적용되지 않는다(모듈 헤더 참조) —
    /// 그래서 쿠키가 없어도 401이 아니라 404여야 한다.
    #[tokio::test]
    async fn unknown_path_is_404() {
        let (status, _, _) = call("/api/jobs").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// 서버 내부 경로(state 디렉터리·config 위치)가 마크업으로 새지 않는다.
    #[tokio::test]
    async fn markup_does_not_leak_server_paths() {
        let ctx = ctx();
        let state = ctx.state_dir.display().to_string();
        let body = call_authed_with(ctx, "/").await;
        assert!(
            !body.contains(&state),
            "state 경로가 HTML로 노출됐다: {state}"
        );
    }

    /// 인증이 설정되지 않은 채 `serve()`를 부르면 포트를 열기 전에 거부한다(비-0 종료,
    /// 명확한 메시지). `TcpListener::bind`보다 먼저 확인하므로 이 테스트는 실제 포트를
    /// 소비하지 않는다.
    ///
    /// `await_holding_lock`을 허용한다 — `#[tokio::test]` 기본값은 `current_thread`
    /// 런타임이라 이 테스트 안에는 애초에 동시 실행되는 다른 태스크가 없다(그래서 std
    /// `Mutex`를 async 전용으로 바꾸지 않아도 이 테스트 프로세스 안에서 데드락 여지가
    /// 없다). 진짜 보호 대상은 **다른 테스트 함수가 별도 OS 스레드에서** 같은 env를
    /// 건드리는 경우이고, 그건 이 가드가 정확히 막는다 — `serve()`가 env를 읽는 동기
    /// 구간(첫 poll에서 `TcpListener::bind`보다 먼저 실행) 동안 잠금을 놓지 않아야
    /// 그 경합을 막을 수 있으므로, await 전에 풀어버리면 보호 목적 자체가 무너진다.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn serve_rejects_when_auth_not_configured() {
        let _guard = auth::ENV_GUARD.lock().unwrap();
        std::env::remove_var(auth::ENV_WEB_TOKEN);
        std::env::remove_var(auth::ENV_WEB_TOKEN_FILE);

        let err = serve(ctx_owned())
            .await
            .expect_err("인증 없이 기동되면 안 됨");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(
            err.to_string().contains(auth::ENV_WEB_TOKEN),
            "무엇을 설정해야 하는지 메시지에 있어야 함: {err}"
        );
    }

    /// 인증이 설정되지 않으면 `--allow-remote`로 얻은 `0.0.0.0` 바인딩조차 애초에 뜨지
    /// 않는다 — 바인딩 가드([`crate::web::resolve_bind`])를 통과했다고 해서 무인증 노출이
    /// 가능해지지 않는다는 것을 고정한다. 이 검사도 bind보다 먼저 실행되므로 실제로
    /// `0.0.0.0`에 소켓을 열지 않는다(샌드박스에서도 항상 안전하게 돈다).
    ///
    /// `await_holding_lock` 허용 근거는 위 테스트와 동일.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn serve_rejects_wildcard_bind_without_auth_configured() {
        let _guard = auth::ENV_GUARD.lock().unwrap();
        std::env::remove_var(auth::ENV_WEB_TOKEN);
        std::env::remove_var(auth::ENV_WEB_TOKEN_FILE);

        let mut cfg = ctx_owned();
        cfg.bind = SocketAddr::from(([0, 0, 0, 0], 8787));
        let err = serve(cfg)
            .await
            .expect_err("0.0.0.0 + 인증 미설정은 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    /// `ctx()`는 `Arc<ServeConfig>`를 돌려주지만 `serve()`는 소유 값을 받는다 — 테스트
    /// 전용으로 값을 복제해 만든다(`ServeConfig`는 `Clone`).
    fn ctx_owned() -> ServeConfig {
        (*ctx()).clone()
    }
}
