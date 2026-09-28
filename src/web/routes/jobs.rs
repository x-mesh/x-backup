//! `GET /jobs`·`GET /jobs/{id}` — 잡 이력 목록과 상세.
//!
//! ## 왜 이 화면은 자식 프로세스를 띄우지 않는가
//! 이 콘솔의 다른 화면은 대개 `x-backup <cmd> --json`을 자식으로 띄워 그 출력을 그린다
//! ([`crate::web`] 최상위 불변식). 여기는 **유일하게 그럴 수 없는 화면**이다 — "누가 언제
//! 어떤 잡을 눌렀고 그 로그가 무엇이었나"는 destination의 manifest에도, 어떤 CLI 명령의
//! 출력에도 없다. 그래서 이 화면만 서버가 자기 손으로 남긴 로컬 상태를 읽는다
//! ([`crate::web::state::jobs`]).
//!
//! 그 예외를 **읽기만으로** 묶는 것이 이 파일의 규율이다:
//!
//! - [`JobStore::attach`]로 붙는다(`open`이 아니다) — GET이 디렉터리를 만들지 않는다.
//! - 저장소의 읽기 API는 실패를 반환하지 않는다. 상태 파일이 없거나 깨져도 화면은 200으로
//!   "이력 없음"을 보여준다(캐시 성격 — [`crate::web::state`] 모듈 헤더).
//! - 잡을 **시작하는** 경로는 여기 없다. 그건 파괴적 작업 화면(t21·t23~25)이 감사 게이트를
//!   지나 하는 일이다.
//!
//! ## 인증 뒤 화면이다
//! 잡 이력은 프로파일 이름·명령·인자를 그대로 보여준다 — `/doctor`가 config 구조를 드러내는
//! 것과 같은 종류의 지도다([`crate::web::server::app_routes`] 주석). 읽기 전용이라는 것이
//! 공개해도 된다는 뜻은 아니다.
//!
//! ## 경로 파라미터를 믿지 않는다
//! `{id}`는 브라우저에서 오는 문자열이고, 그 값이 **파일명이 될 자리**다.
//! [`JobId::parse`]가 UUID 문법만 통과시키므로 `..`·`/`·널 바이트는 파일시스템에 닿기
//! 전에 400으로 끊긴다. 파싱 실패(400)와 "그런 잡이 없음"(404)을 구분하는 이유: 앞은
//! 링크가 잘못된 것이고, 뒤는 로테이션·다른 state 디렉터리를 의심할 일이다.
//!
//! ## 마스킹
//! 잡 로그는 자식 프로세스가 찍은 임의의 텍스트다. [`request_registry`]가 만든
//! [`SecretRegistry`]를 화면이 한 번 더 통과시킨다 — 쓰는 쪽(t11)이 이미 지웠더라도
//! 마지막 방어선을 화면에 둔다([`crate::web::view::jobs`] 헤더).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use maud::Markup;

use crate::web::mask::SecretRegistry;
use crate::web::state::jobs::{JobId, JobStore};
use crate::web::view::{jobs as view, layout};
use crate::web::ServeConfig;

/// 목록 화면 경로. 라우터·마크업·테스트가 이 상수를 공유한다.
pub const JOBS_PATH: &str = "/jobs";

/// 상세 화면의 **라우터 패턴**(axum 0.8의 `{name}` 문법). 링크 문자열은 이 패턴이 아니라
/// [`job_detail_href`]로 만든다 — 패턴을 그대로 href에 쓰면 `{id}`가 URL에 남는다.
pub const JOB_DETAIL_ROUTE: &str = "/jobs/{id}";

/// `<title>`·화면 제목·내비게이션 라벨이 공유하는 이름. 기술용어이므로 영문 고정
/// ([`crate::i18n`] 규약). [`layout`]이 현재 화면 판정에 이 값을 쓴다 — **상세 화면도 같은
/// 제목을 쓴다**(둘은 같은 내비게이션 항목에 속한다).
pub const JOBS_TITLE: &str = "Jobs";

/// 상세 화면 링크. [`JobId`]의 정규형만 들어가므로 이스케이프가 필요한 문자가 나올 수 없다
/// (하이픈과 hex뿐이다).
pub fn job_detail_href(id: &JobId) -> String {
    format!("{JOBS_PATH}/{id}")
}

/// 이 요청에서 쓸 시크릿 레지스트리.
///
/// 잡 러너가 자식에게 주입하는 시크릿 값들이 이미 등록되어 있으므로
/// ([`crate::web::job::JobSecrets`]), 그것을 복제해 쓰고 콘솔 세션 토큰만 얹는다. 토큰은
/// 자식에게 전달되지 않지만(러너의 env 화이트리스트), 우리 프로세스가 만드는 텍스트에
/// 우연히 섞이는 경로까지 막는다 — `routes::doctor::request_registry`와 같은 판단이다.
///
/// "XB_로 시작하는 값 전부"처럼 넓게 잡지 않는 이유도 그쪽과 같다: 과잉 마스킹은 로그
/// 판독성을 잡아먹고, 읽을 수 없는 화면은 운영자를 CLI로 돌려보낸다.
fn request_registry(ctx: &ServeConfig) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    crate::web::mask::register_from_env_names(&mut registry, [crate::web::auth::ENV_WEB_TOKEN]);
    registry
}

/// `GET /jobs` — 이력 목록.
pub async fn page(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let history = JobStore::attach(&ctx.state_dir).list().await;
    let body = view::list_body(ctx.lang, &history, &request_registry(&ctx));
    layout::shell(ctx.lang, JOBS_TITLE, body)
}

/// `GET /jobs/{id}` — 잡 하나의 인자와 로그.
///
/// 상태 코드가 세 갈래인 이유는 모듈 헤더 "경로 파라미터를 믿지 않는다" 참조.
pub async fn detail(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            // 잘못된 링크다. 입력 문자열을 화면에 되돌려 그리지 않는다 — 남의 문자열을
            // 그대로 반사하는 화면을 만들지 않는 것이 기본이고, 여기서는 진단에도
            // 도움이 되지 않는다(형식이 틀렸다는 사실이 전부다).
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, JOBS_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response();
        }
    };

    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    // 요약도 로그 파일도 없으면 이 서버가 모르는 잡이다.
    let status = if detail.summary.is_none() && !detail.log_file_present {
        StatusCode::NOT_FOUND
    } else {
        StatusCode::OK
    };
    let body = view::detail_body(ctx.lang, &detail, &request_registry(&ctx));
    (status, layout::shell(ctx.lang, JOBS_TITLE, body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::{self, AuthState};
    use crate::web::job::JobOutcome;
    use crate::web::state::jobs::{JobStart, LogStream};
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::middleware;
    use axum::routing::get;
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// 테스트 세션 토큰.
    const TEST_TOKEN: &str = "test-token-jobs-9f2b";

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

    /// 이 화면 두 개만 담은 최소 라우터.
    ///
    /// `server.rs`의 라우터를 쓰지 않는 이유: 이 태스크는 그 파일을 건드리지 않는다(배선은
    /// 리더의 몫). 그래서 **리더가 붙일 모양 그대로** 여기서 조립해, 배선되면 401/200이
    /// 실제로 그렇게 동작한다는 것을 미리 고정한다 — `route_layer`(`layer`가 아니라)까지
    /// 같은 형태다([`crate::web::server`] 헤더의 근거).
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(JOBS_PATH, get(page))
            .route(JOB_DETAIL_ROUTE, get(detail))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .with_state(ctx)
    }

    /// 요청 하나를 보낸다. `cookie`가 `Some`이면 세션 쿠키를 싣는다.
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

    /// 이력 한 건을 심은 테스트 컨텍스트를 만든다.
    async fn ctx_with_one_job() -> (Arc<ServeConfig>, JobId) {
        let ctx = Arc::new(ServeConfig::for_test());
        let store = JobStore::open(&ctx.state_dir);
        let args = vec![
            "backup".to_string(),
            "--profile".to_string(),
            "prod".to_string(),
        ];
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &args,
                started_at: chrono::Utc::now(),
                pid: Some(4242),
            })
            .await
            .expect("이력 기록 실패");
        store
            .append_log(&id, LogStream::Stderr, "dumping collection users")
            .await
            .unwrap();
        store.finish(&id, JobOutcome::Succeeded).await.unwrap();
        (ctx, id)
    }

    /// 두 화면 모두 쿠키 없이는 401이고, 내용을 흘리지 않는다.
    #[tokio::test]
    async fn both_screens_require_auth() {
        let (ctx, id) = ctx_with_one_job().await;
        for uri in [JOBS_PATH.to_string(), job_detail_href(&id)] {
            let (status, body) = call(Arc::clone(&ctx), &uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}가 열려 있다");
            assert!(
                !body.contains(JOBS_TITLE),
                "미인증 응답이 화면을 흘렸다: {body}"
            );
            assert!(!body.contains("prod"), "미인증 응답이 프로파일을 흘렸다");
            assert!(!body.contains(TEST_TOKEN), "토큰이 응답에 노출됐다");
        }
        // 틀린 쿠키도 401 — 쿠키가 "있기만" 하면 통과하는 사고를 막는다.
        let (status, _) = call(ctx, JOBS_PATH, Some("not-the-token")).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    /// 쿠키가 있으면 목록이 200 + 껍데기와 함께 렌더된다.
    #[tokio::test]
    async fn list_renders_with_cookie() {
        let (ctx, id) = ctx_with_one_job().await;
        let (status, body) = call(ctx, JOBS_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.starts_with("<!DOCTYPE html>"), "문서 골격 누락");
        assert!(
            body.contains("<title>x-backup — Jobs</title>"),
            "화면 제목 누락: {body}"
        );
        assert!(body.contains(&job_detail_href(&id)), "상세 링크 누락");
        assert!(body.contains("backup"), "명령 누락");
        assert!(body.contains(r#"data-level="ok""#), "상태 배지 누락");
    }

    /// 상세도 200이고 로그 본문이 실린다.
    #[tokio::test]
    async fn detail_renders_with_cookie() {
        let (ctx, id) = ctx_with_one_job().await;
        let (status, body) = call(ctx, &job_detail_href(&id), Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains(&id.to_string()), "잡 id 누락");
        assert!(
            body.contains("dumping collection users"),
            "로그 누락: {body}"
        );
        assert!(body.contains("--profile prod"), "인자 누락");
    }

    /// 이력이 없는 서버에서도 목록이 200 + "이력 없음"을 낸다(패닉·500 없음).
    #[tokio::test]
    async fn list_is_ok_when_state_dir_has_no_history() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = call(ctx, JOBS_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("No job history yet"),
            "안내 문장 누락: {body}"
        );
    }

    /// state 디렉터리 자체가 사라져도 목록이 200이다 — 상태 손상이 화면을 죽이지 않는다.
    #[tokio::test]
    async fn list_survives_a_destroyed_state_dir() {
        let ctx = Arc::new(ServeConfig::for_test());
        std::fs::remove_dir_all(&ctx.state_dir).expect("테스트 state 디렉터리 제거 실패");
        let (status, body) = call(Arc::clone(&ctx), JOBS_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("No job history yet"));
        // 읽기 경로는 사라진 디렉터리를 다시 만들지 않는다.
        assert!(
            !ctx.state_dir.exists(),
            "GET 요청이 state 디렉터리를 만들었다"
        );
    }

    /// 손상된 인덱스여도 화면은 200이고, 읽지 못한 줄이 있다는 사실을 말한다.
    #[tokio::test]
    async fn corrupt_index_still_renders() {
        let ctx = Arc::new(ServeConfig::for_test());
        let store = JobStore::open(&ctx.state_dir);
        std::fs::write(store.index_path(), b"{not json\n{\"kind\":\"nope\"}\n").unwrap();
        let (status, body) = call(ctx, JOBS_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("Partial history"),
            "부분 읽기 알림 누락: {body}"
        );
    }

    /// 형식이 틀린 id는 400이고, 경로 조작 문자열이 파일시스템에 닿지 않는다.
    #[tokio::test]
    async fn malformed_id_is_rejected() {
        let ctx = Arc::new(ServeConfig::for_test());
        for raw in ["not-a-uuid", "..", "index", "0190f0a2-1a2b-7c3d-8e4f"] {
            let (status, body) = call(
                Arc::clone(&ctx),
                &format!("{JOBS_PATH}/{raw}"),
                Some(session()),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "'{raw}'가 통과했다");
            // 입력 문자열을 되돌려 그리지 않는다.
            assert!(!body.contains(raw), "입력이 화면에 반사됐다: {body}");
        }
    }

    /// 문법은 맞지만 없는 잡은 404 + 읽을 수 있는 설명이다.
    #[tokio::test]
    async fn unknown_job_is_404_with_readable_page() {
        let ctx = Arc::new(ServeConfig::for_test());
        let missing = JobId::generate();
        let (status, body) = call(ctx, &job_detail_href(&missing), Some(session())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert!(body.starts_with("<!DOCTYPE html>"), "404도 화면이어야 한다");
        assert!(body.contains("Unknown job"), "설명 누락: {body}");
    }

    /// 화면이 서버 내부 경로(state 디렉터리)를 흘리지 않는다.
    #[tokio::test]
    async fn markup_does_not_leak_server_paths() {
        let (ctx, id) = ctx_with_one_job().await;
        let state = ctx.state_dir.display().to_string();
        for uri in [JOBS_PATH.to_string(), job_detail_href(&id)] {
            let (_, body) = call(Arc::clone(&ctx), &uri, Some(session())).await;
            assert!(
                !body.contains(&state),
                "state 경로가 HTML로 노출됐다: {state}"
            );
        }
    }

    /// 로그에 섞인 시크릿이 응답에서 마스킹된다 — 러너 레지스트리를 실제로 경유하는지 본다.
    #[tokio::test]
    async fn secrets_in_logs_are_masked_in_the_response() {
        const FAKE: &str = "NOT-A-REAL-SECRET-jobsroute-3c7e15a9";
        let ctx = Arc::new(ServeConfig::for_test());
        let store = JobStore::open(&ctx.state_dir);
        let id = store
            .start(JobStart {
                command: "backup",
                profile: None,
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: None,
            })
            .await
            .unwrap();
        store
            .append_log(&id, LogStream::Stderr, &format!("auth failed with {FAKE}"))
            .await
            .unwrap();
        store.finish(&id, JobOutcome::Failed).await.unwrap();

        // 러너 레지스트리에 그 값을 등록한 컨텍스트를 만든다(실제로는 config의 `*_env`가
        // 채운다 — `JobSecrets::load`).
        let mut secrets = crate::web::job::JobSecrets::new();
        secrets.insert_value("XB_TEST_JOBS_ROUTE_URI", FAKE);
        let mut cfg = (*ctx).clone();
        cfg.jobs = Arc::new(
            crate::web::job::JobRunner::new(None, cfg.lang, secrets).expect("러너 생성 실패"),
        );
        let ctx = Arc::new(cfg);

        let (status, body) = call(ctx, &job_detail_href(&id), Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(!body.contains(FAKE), "시크릿 원문이 응답에 남았다");
        assert!(
            body.contains(crate::web::mask::REDACTED_PLACEHOLDER),
            "마스킹 표식이 없다: {body}"
        );
    }
}
