//! 웹 콘솔 시크릿 누출 회귀 테스트 — 알려진 시크릿 값을 프로세스 env에 심어 두고,
//! 라우터의 모든 알려진 엔드포인트 응답(헤더 + 본문)을 훑어 원문이 한 번도 등장하지
//! 않음을 단정한다.
//!
//! ## 이 하네스는 한때 아무것도 검사하지 않았다 (그리고 초록색이었다)
//! 이 파일을 고칠 때 반드시 알아야 할 이력이다. 이전 판본은 세 겹으로 죽어 있었다:
//!
//! 1. **쿠키를 싣지 않았다.** [`known_routes`]의 9개 중 `/healthz`·`/assets/app.css`를 뺀
//!    전부가 `require_auth`에서 401 plain-text로 끊겼다 — 하네스는 **렌더된 화면 본문을 한
//!    번도 보지 않았다.** 그런데 파일 헤더에는 "401도 유효한 '시크릿 누출 없음'"이라고 적혀
//!    있어서, 그 상태가 정상으로 보였다.
//! 2. **심은 env를 아무도 참조하지 않았다.** `XB_WEBLEAKTEST_MONGO_URI`를 가리키는 config가
//!    없었으므로, 라우트가 그 값을 화면에 실을 경로 자체가 존재하지 않았다.
//! 3. **컨텍스트가 비어 있었다.** `ServeConfig::for_test()`는 `config_path: None` +
//!    `JobSecrets::new()`(레지스트리 빈 상태)다. 마스킹 대상이 0건이니 마스킹이 동작하는지도
//!    확인할 수 없었다.
//!
//! 그래서 라우트가 인증 뒤 본문에 원문을 흘리기 시작해도 이 테스트는 계속 통과했을 것이다.
//! 지금은 세 겹을 모두 메웠고, **하네스 자신이 살아 있는지**를 카나리로 증명한다
//! ([`harness_catches_a_value_that_reaches_the_screen`]) — 그 테스트가 없으면 "죽었는데
//! 초록"이 언제든 재발한다.
//!
//! ## 라우트를 추가할 때 해야 할 일
//! `axum::Router`는 등록된 경로를 열거하는 공개 API가 없다(matchit 기반 내부 라우팅
//! 트리가 비공개). 그래서 이 목록은 자동 열거가 아니라 [`known_routes`]에 수동으로
//! 동기화한다 — **새 라우트를 추가하면 이 함수에 경로를 한 줄 추가하라.** 라우트가
//! `pub const` 경로 상수를 노출하면(`server::HEALTHZ_PATH`처럼) 그 상수를 참조하고,
//! 그렇지 않으면 리터럴 문자열에 짧은 주석을 남겨라. [`known_routes_are_registered`]가
//! 목록과 실제 라우터 사이의 드리프트(오타·삭제된 경로)를 얕게나마 잡아준다 — 그
//! 테스트가 확인하는 건 "404가 아니다"뿐이라 완전하지 않다는 점에 주의.
//!
//! ## 이 테스트가 커버하는 것 / 못 하는 것 (정직하게)
//! - 커버: [`known_routes`]의 모든 경로를 **인증된 세션으로** 호출해, 응답 헤더 + 본문에
//!   env로 주입한 더미 시크릿 원문이 등장하지 않는지. 인증이 필요한 경로는 200이면서 실제로
//!   렌더된 화면인지도 함께 단정한다([`assert_rendered_page`]) — 그 단정이 없으면 401·빈
//!   본문으로 되돌아가도 이 파일이 조용히 통과한다.
//! - 못 커버: 잡 로그·SSE 스트림 — HTTP 응답 경로가 아니라 별도 파일/이벤트 스트림이라
//!   이 하네스가 보지 않는다. 감사 로그(t8) — 파일에 append되는 구조라 마찬가지다. 이들은
//!   각 태스크가 자체 테스트를 갖춰야 한다.
//! - 못 커버: 경로 파라미터가 필요한 라우트(`/jobs/{id}`·`/backup/{job_id}`)와 POST 제출.
//!   그쪽 마스킹은 해당 라우트 모듈의 단위 테스트가 고정한다.
//! - 못 커버: [`x_backup::web::mask::SecretRegistry`]의 마스킹 경계(URL 인코딩·재인코딩
//!   등으로 변형된 시크릿을 못 잡는 것) — 그건 `src/web/mask.rs`의 `adversarial_*`
//!   단위 테스트가 다룬다. 이 파일은 "현재 라우트가 원문을 그대로 흘리지 않는가"만 본다.
//!
//! ## 왜 실제 프로세스를 spawn하지 않고 라우터를 직접 호출하는가
//! `src/web/server.rs`의 기존 테스트(`router(ctx(), auth()).oneshot(...)`)와 같은
//! 패턴이다 — 포트 바인딩·타이밍에 흔들리지 않는다. 그 대신 이 테스트는 TCP/HTTP
//! 프레이밍 레벨(hyper가 붙이는 헤더 등)이나 기동 배너(`println!`)는 보지 않는다 —
//! 배너의 경로 비노출은 `server.rs::markup_does_not_leak_server_paths`가 이미 다룬다.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderMap, Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

use x_backup::config::file::Config;
use x_backup::i18n::Lang;
use x_backup::web::auth::{self, AuthState};
use x_backup::web::job::{JobRunner, JobSecrets};
use x_backup::web::{server, view, ServeConfig};

/// 이 파일 안에서 프로세스 env를 건드리는 테스트끼리 직렬화한다. env는 프로세스
/// 전역 상태라 같은 테스트 바이너리 안의 병렬 스레드끼리 값을 덮어쓸 수 있다 —
/// `src/web/auth.rs`가 자기 테스트끼리 쓰는 `ENV_GUARD`와 같은 이유의 같은 해법이다
/// (그건 `pub(crate)`라 이 통합 테스트 크레이트에서 재사용할 수 없어 별도로 둔다).
/// **테스트 함수 전체를 감싸라** — 이제 라우트가 실제로 env를 읽으므로(config가 심은
/// 이름을 가리킨다), 부분적으로 감싸면 다른 스레드의 `set_var`/`remove_var`와 실제로
/// 경합한다. [`harness_catches_a_value_that_reaches_the_screen`]은 패닉 훅도 잠시
/// 갈아끼우므로(프로세스 전역) 이 직렬화가 그쪽에서도 필요하다.
///
/// `std::sync::Mutex`가 아니라 `tokio::sync::Mutex`를 쓴다 — 이 잠금이 `fetch`의
/// `.await` 구간을 통째로 감싸므로, 동기 뮤텍스를 쓰면 await 지점 사이에 락을 쥔 채
/// 넘어가 clippy(`await_holding_lock`)가 걸린다(async 런타임에서 실제로 데드락
/// 위험도 있다).
static ENV_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 명백히 가짜인 더미 시크릿 — 실제처럼 보이되 파일에 커밋돼도 진짜 자격증명으로
/// 오인될 수 없도록 접두어를 박아 둔다.
const FAKE_MONGO_PASSWORD: &str = "NOT-A-REAL-SECRET-x7qP2vLk9mZs4wRt-webleaktest";
const FAKE_S3_SECRET_KEY: &str = "NOT-A-REAL-SECRET-aB3dE6fG9hJ2kM5nQr-webleaktest";

/// 시크릿 값을 담을 env 변수 이름. **아래 [`config_toml`]이 이 이름들을 실제로 참조한다** —
/// 참조하지 않으면 값이 화면에 도달할 경로가 없어 이 하네스가 아무것도 검사하지 않게 된다
/// (파일 헤더의 2번).
const ENV_MONGO_URI: &str = "XB_WEBLEAKTEST_MONGO_URI";
const ENV_S3_CREDS: &str = "XB_WEBLEAKTEST_S3_CREDS";

/// 프로파일 이름으로 심는 카나리 — **마스킹 대상이 아니므로 화면에 그대로 렌더된다.**
///
/// 이 값의 목적은 시크릿 보호가 아니라 **하네스 자신의 생존 확인**이다: 실제 라우트가 실제로
/// 그린 본문에서 이 문자열이 발견되어야 "이 하네스는 인증 뒤 본문을 본다"가 증명되고, 같은
/// 응답에 [`assert_no_leak`]을 걸면 반드시 실패해야 "이 하네스는 누출을 잡는다"가 증명된다.
///
/// 프로파일 이름 규칙(ASCII 영문·숫자·`_`·`-`, 영문/숫자로 시작, 64바이트 이하)을 만족해야
/// config가 파싱된다.
const CANARY_PROFILE: &str = "NOT-A-REAL-SECRET-canary-x7qP2vLk9mZs4wRt";

/// 라우터 호출에 필요한 인증 토큰.
///
/// `AuthState::load_from_env`가 요구하는 형식(쿠키에 안전한 문자)과 **강도 하한**
/// (`auth::MIN_TOKEN_LEN`·`MIN_TOKEN_ENTROPY_BITS`)을 만족해야 한다 — 짧거나 단조로운
/// 토큰은 이제 기동 자체가 거부된다.
const DUMMY_AUTH_TOKEN: &str = "webleaktest-harness-dummy-token-not-a-real-secret";

/// 심은 env 이름을 실제로 참조하는 config. v1(중첩) 문법을 쓴다 — 이 파일이 검증하려는 것은
/// 문법이 아니라 "시크릿이 화면에 닿을 수 있는 상태"이고, v1 표기가 `credentials_env`까지
/// 한눈에 드러나 읽기 쉽다.
fn config_toml() -> String {
    format!(
        r#"
default_profile = "{CANARY_PROFILE}"

[profiles.{CANARY_PROFILE}.source]
uri_env = "{ENV_MONGO_URI}"

[profiles.{CANARY_PROFILE}.destination]
type = "s3"

[profiles.{CANARY_PROFILE}.destination.s3]
bucket          = "xb-webleaktest"
credentials_env = "{ENV_S3_CREDS}"
"#
    )
}

/// 이 하네스가 스캔하는 전체 라우트 목록. 새 라우트가 생기면 여기 한 줄을 추가하라
/// (파일 헤더 "라우트를 추가할 때 해야 할 일" 참고).
fn known_routes() -> Vec<String> {
    vec![
        "/".to_string(), // index — server.rs가 경로 상수를 노출하지 않아 리터럴로 등록
        server::HEALTHZ_PATH.to_string(),
        // doctor 화면(t9). 인증 쿠키를 싣게 된 뒤로는 자식 프로세스가 **실제로** 뜬다 —
        // 자기 자신(`current_exe()`)이 이 테스트 바이너리라 doctor로서 동작하지는 않지만,
        // 그 실패 화면까지가 이 하네스의 검사 대상이다(자식 stderr 발췌가 본문에 실린다).
        x_backup::web::routes::doctor::DOCTOR_PATH.to_string(),
        // 락 현황 화면(t13). pid·hostname·프로파일명을 보여주는 화면이라 시크릿이 섞일
        // 여지가 특히 없어야 한다 — 락 파일 절대 경로를 노출하지 않는 것도 t13의 결정이다
        // (`XDG_RUNTIME_DIR` 실제 값이 드러나는 것을 막는다).
        x_backup::web::routes::lock::LOCK_PATH.to_string(),
        // 잡 이력 목록(t14). 잡 인자와 자식 stderr가 흘러드는 화면이라 이 하네스가
        // 가장 값어치를 하는 자리다. 상세(`/jobs/{id}`)는 경로 파라미터가 필요해
        // 여기 목록에 넣지 않는다 — 대신 `routes::jobs`의 단위 테스트가 마스킹을 고정한다.
        x_backup::web::routes::jobs::JOBS_PATH.to_string(),
        // config 편집(t26). 시크릿을 **값이 아니라 env 이름만** 다루는 화면이라 이 하네스의
        // 핵심 대상이다 — 값 필드가 응답 스키마에 아예 없다는 것이 1차 방어이고, 이 테스트가
        // 그 방어가 실제로 성립하는지를 끝단에서 확인한다.
        x_backup::web::routes::config::CONFIG_PATH.to_string(),
        x_backup::web::routes::config::NEW_PATH.to_string(),
        // 백업 실행 폼(t15). 프로파일 목록을 렌더하는 화면이라 source URI가 스치는 자리다.
        // 실행 중 화면·SSE·취소는 job_id가 필요해 이 목록에 넣지 않는다 — 그 경로들의
        // 마스킹은 `routes::backup`의 단위 테스트와 t11의 `JobHub::mask`가 고정한다.
        x_backup::web::routes::backup::BACKUP_PATH.to_string(),
        // 대시보드(t16). 프로파일마다 자식을 띄워 그 stdout/stderr를 한 화면에 모으는
        // 구조라, 마스킹이 뚫리면 여러 프로파일의 URI가 **동시에** 새는 화면이다.
        x_backup::web::routes::dashboard::DASHBOARD_PATH.to_string(),
        // 라이브 모니터 화면(t18). 첫 렌더에는 측정값이 아예 없지만(프레임은 SSE로 온다),
        // 그 사실 자체를 이 스윕이 고정한다 — 나중에 "초기값을 서버가 채우자"는 변경이
        // 들어오면 자식 stdout이 본문에 실리게 되고, 그 순간 이 경로가 검사 대상이 된다.
        // SSE(`/monitor/events`)는 응답이 끝나지 않는 스트림이라 이 GET 스윕이 보지 않는다.
        x_backup::web::routes::monitor::MONITOR_PATH.to_string(),
        // 카탈로그(t17). 자식 `list --json`을 파싱해 행으로 펴는 화면이다 — 매니페스트에
        // 스민 값이 그대로 표에 실릴 수 있는 자리.
        x_backup::web::routes::catalog::CATALOG_PATH.to_string(),
        // peek(t20). DB 내용을 보여주는 것이 목적인 화면이므로 "무엇을 안 보여주는가"가
        // 곧 방어다. 파라미터 없는 피커 화면만 여기서 훑는다 — 원문 노출(`/peek/reveal`)은
        // POST이고 감사 기록을 요구해 이 하네스의 GET 스윕 대상이 아니다.
        x_backup::web::routes::peek::PEEK_PATH.to_string(),
        // verify 폼(t22). 프로파일 목록을 렌더한다. 결과 화면(`/verify/{job_id}`)은 경로
        // 파라미터가 필요해 제외 — 그쪽 마스킹은 `routes::verify` 단위 테스트가 고정한다.
        x_backup::web::routes::verify::VERIFY_PATH.to_string(),
        // prune 폼(t23). 프로파일 목록을 렌더하는 화면이라 source URI가 스치는 자리다.
        // 계획·실행은 POST이고 자식을 띄우므로 이 GET 스윕 대상이 아니다.
        x_backup::web::routes::prune::PRUNE_PATH.to_string(),
        // migrate 폼(t24). 프로파일 드롭다운을 두 개 렌더하는 화면이라 source/target 양쪽의
        // URI가 스치는 자리다.
        x_backup::web::routes::migrate::MIGRATE_PATH.to_string(),
        // restore 폼(t25). 이 콘솔에서 가장 위험한 화면이라 프로파일 목록이 두 번(읽을 곳·
        // 복구할 곳) 렌더된다 — source URI가 스치는 자리도 그만큼 넓다.
        x_backup::web::routes::restore::RESTORE_PATH.to_string(),
        // 스케줄(t29). 목록과 새 폼 둘 다 프로파일명을 다루고, 저장된 스케줄이 인자를
        // 통째로 들고 있어 시크릿이 인자에 섞이면 여기로 흘러나온다.
        x_backup::web::routes::schedule::SCHEDULE_PATH.to_string(),
        x_backup::web::routes::schedule::NEW_PATH.to_string(),
        view::app_css_url(), // 지문(fingerprint) 쿼리가 붙은 실제 서빙 URL
    ]
}

/// 인증 없이 열리는 경로인가(`server::public_routes`에 실린 것들).
///
/// 이 목록이 필요한 이유: 나머지 경로에는 "인증 뒤 실제로 렌더된 화면"을 요구해야 하는데,
/// 헬스체크와 스타일시트는 애초에 화면이 아니다.
fn is_public(route: &str) -> bool {
    route == server::HEALTHZ_PATH || route.starts_with(view::APP_CSS_PATH)
}

/// 테스트용 `ServeConfig` — 실제 바인딩은 하지 않으므로 주소는 형식만 유효하면 된다.
///
/// `ServeConfig::for_test()`를 **바탕으로 쓰고 필요한 두 필드만 덮어쓴다.** 구조체를 손으로
/// 조립하지 않는 이유는 그 생성자의 doc이 이미 설명한다 — 필드가 하나 늘 때마다 이 파일이
/// 깨지고, 그때 "컴파일되는 아무 값"으로 메우면 이 하네스는 서버가 실제로 쓰는 것과 다른
/// 컨텍스트를 검사하면서 통과한다. 반대로 두 필드를 덮어쓰지 **않으면** 파일 헤더의 2·3번
/// (config 미연결·빈 레지스트리)이 그대로 남아 아무것도 검사하지 못한다.
///
/// [`JobSecrets::load`]를 쓰는 것이 요점이다 — 서버 기동 경로(`web::handle`)와 **같은
/// 함수**로 시크릿을 모으므로, 마스킹 레지스트리가 프로덕션과 같은 내용으로 채워진다.
/// 그 호출이 심은 env를 프로세스 환경에서 제거하는 것도 프로덕션과 같다(그래서 이후
/// 라우트는 값을 env에서 다시 읽을 수 없고, 자식에게만 주입된다).
///
/// **이 하네스는 잡을 실행하지 않는다** — 라우터가 state를 요구하기 때문에 채우는 것뿐이다.
/// (`/doctor`만 예외적으로 자식을 띄운다 — [`known_routes`]의 주석 참조.)
fn ctx(config_path: &Path) -> Arc<ServeConfig> {
    let text = std::fs::read_to_string(config_path).expect("테스트 config를 읽을 수 없다");
    let parsed = Config::from_toml_str(&text).expect("테스트 config가 파싱되지 않는다");
    let secrets = JobSecrets::load(Some(&parsed));
    // 하한(`>=`)으로 본다 — `load`는 config가 선언한 두 개 외에 `XB_AES_KEY_HEX`도 가져가므로
    // 그 값이 설정된 환경에서 돌리면 3이 된다. 확인하고 싶은 것은 "비어 있지 않다"이고,
    // 빈 레지스트리는 이 하네스를 죽이는 세 원인 중 하나였다(파일 헤더의 3번).
    assert!(
        secrets.registry().len() >= 2,
        "심은 시크릿 두 개가 마스킹 대상으로 등록되지 않았다 — 이 하네스가 검사할 것이 없다"
    );

    let mut cfg = ServeConfig::for_test();
    cfg.config_path = Some(config_path.to_path_buf());
    cfg.jobs = Arc::new(
        JobRunner::new(Some(config_path.to_path_buf()), Lang::En, secrets)
            .expect("테스트 잡 러너 생성 실패"),
    );
    Arc::new(cfg)
}

/// config 파일을 임시 디렉터리에 쓰고 그 경로를 돌려준다.
///
/// 정리하지 않는다 — `ServeConfig::for_test()`가 state 디렉터리를 남기는 것과 같은 판단이다
/// (임시 디렉터리에 작은 파일 하나가 남을 뿐이고, `tempfile::TempDir`를 들고 있으면 반환
/// 타입에 수명이 붙어 호출부가 번거로워진다).
fn write_config(tag: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("x-backup-webleaktest-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("임시 디렉터리 생성 실패");
    let path = dir.join("config.toml");
    std::fs::write(&path, config_toml()).expect("config 쓰기 실패");
    path
}

/// 시크릿 값을 프로세스 env에 심는다(config가 이 이름들을 참조한다).
fn plant_secrets() {
    std::env::set_var(
        ENV_MONGO_URI,
        format!("mongodb://backupuser:{FAKE_MONGO_PASSWORD}@fake-host.example:27017/prod"),
    );
    // src/storage/s3.rs의 `credentials_env` 형식("ACCESS:SECRET")을 따른다.
    std::env::set_var(
        ENV_S3_CREDS,
        format!("AKIAFAKEFAKEFAKE12Q:{FAKE_S3_SECRET_KEY}"),
    );
}

/// 라우터가 요구하는 `Arc<AuthState>`를 즉석에서 만든다.
///
/// `AuthState`의 테스트 전용 생성자(`for_test`)는 `pub(crate)`라 외부 크레이트인 이
/// 통합 테스트에서는 못 쓴다 — 유일한 공개 생성 경로인 `load_from_env()`를 문서화된
/// 방식대로(토큰 env를 심고 읽고 지우고) 호출한다.
///
/// **호출자가 이미 [`ENV_LOCK`]을 쥐고 있어야 한다** — 이 함수 자체는 잠그지 않는다
/// (테스트 함수가 처음부터 끝까지 하나의 락 구간을 유지하게 하기 위해서다 — 파일
/// 헤더의 `ENV_LOCK` 문서 참고).
fn auth_state_locked() -> Arc<AuthState> {
    std::env::set_var(auth::ENV_WEB_TOKEN, DUMMY_AUTH_TOKEN);
    let state = AuthState::load_from_env().expect("테스트 토큰으로 AuthState 생성 실패");
    std::env::remove_var(auth::ENV_WEB_TOKEN);
    Arc::new(state)
}

/// 라우터를 포트 없이 직접 호출해 상태 코드 + 헤더 + 본문을 모은다.
///
/// **유효한 세션 쿠키를 반드시 싣는다.** 쿠키가 없으면 보호된 라우트가 401 plain-text로
/// 끊기고, 그러면 이 하네스는 화면 본문을 한 번도 보지 못한다(파일 헤더의 1번).
///
/// 쿠키에 실리는 것은 토큰이 아니라 `auth`가 발급한 **세션 ID**다 — 토큰을 넣으면 통과하지
/// 않는다(`crate::web::auth` 모듈 헤더 "쿠키 값은 토큰이 아니다"). 그래서 세션은 라우터가
/// 보는 것과 **같은** `AuthState`에서 뽑아야 한다.
async fn fetch(
    ctx: &Arc<ServeConfig>,
    auth: &Arc<AuthState>,
    uri: &str,
) -> (StatusCode, HeaderMap, String) {
    let session = auth.issue_session().expect("테스트 세션 발급 실패");
    let request = Request::builder()
        .uri(uri)
        .header(
            header::COOKIE,
            format!("{}={session}", auth::SESSION_COOKIE_NAME),
        )
        .body(Body::empty())
        .unwrap();
    let response = server::router(ctx.clone(), auth.clone())
        .oneshot(request)
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

/// 헤더+본문 어디에도 주어진 원문이 등장하지 않는지 확인한다.
fn assert_no_leak(route: &str, status: StatusCode, headers: &HeaderMap, body: &str, needle: &str) {
    assert!(
        !body.contains(needle),
        "{route}(status={status}) 응답 본문에 시크릿 원문이 노출됨"
    );
    for (name, value) in headers.iter() {
        let rendered = value.to_str().unwrap_or("<non-utf8 header>");
        assert!(
            !rendered.contains(needle),
            "{route}(status={status}) 헤더 {name:?}에 시크릿 원문이 노출됨: {rendered}"
        );
    }
}

/// 이 응답이 **인증을 통과해 실제로 렌더된 화면**인지 확인한다.
///
/// 이 단정이 이 파일의 부활 장치다. 누출 단정만 있으면 401 plain-text("unauthorized …")도
/// 조용히 통과하므로(시크릿이 없는 것은 사실이다) 커버리지가 0인 상태를 아무도 눈치채지
/// 못한다. 그래서 "200이고, 레이아웃 골격이 들어 있다"까지 요구한다.
fn assert_rendered_page(route: &str, status: StatusCode, body: &str) {
    assert_eq!(
        status,
        StatusCode::OK,
        "{route}가 인증 쿠키로도 200이 아니다 — 하네스가 본문을 검사하지 못한다: {body}"
    );
    assert!(
        body.contains(&view::app_css_url()),
        "{route} 응답이 렌더된 화면이 아니다(레이아웃 골격 없음): {body}"
    );
}

/// 알려진 시크릿을 env에 심어 두고 모든 알려진 라우트의 응답을 훑는다.
///
/// 이제 심은 값은 **실제로 화면에 닿을 수 있다** — config가 그 env 이름을 참조하고, 라우트는
/// 인증을 통과한 상태로 호출되며, 마스킹 레지스트리는 프로덕션과 같은 방식으로 채워져 있다.
/// 그래서 이 테스트의 통과는 자명하지 않다(예전에는 자명했다 — 파일 헤더 참조).
#[tokio::test]
async fn no_known_route_leaks_env_secrets_in_body_or_headers() {
    let _guard = ENV_LOCK.lock().await;
    let auth = auth_state_locked();
    plant_secrets();
    let config_path = write_config("leak");

    let ctx = ctx(&config_path);
    for route in known_routes() {
        let (status, headers, body) = fetch(&ctx, &auth, &route).await;
        if !is_public(&route) {
            assert_rendered_page(&route, status, &body);
        }
        assert_no_leak(&route, status, &headers, &body, FAKE_MONGO_PASSWORD);
        assert_no_leak(&route, status, &headers, &body, FAKE_S3_SECRET_KEY);
    }

    std::env::remove_var(ENV_MONGO_URI);
    std::env::remove_var(ENV_S3_CREDS);
}

/// **이 하네스가 실제로 누출을 잡는다는 증명.**
///
/// 마스킹을 일부러 우회한 라우트를 만드는 대신(그런 코드를 저장소에 남기고 싶지 않다),
/// 마스킹 대상이 **아닌** 값을 화면에 도달하게 만든다: [`CANARY_PROFILE`]을 프로파일
/// 이름으로 심으면 `/config` 목록이 그것을 그대로 렌더한다. 그 응답으로 두 가지를 확인한다:
///
/// 1. 카나리가 본문에 있다 → 이 하네스는 **인증 뒤 렌더된 본문을 본다**(예전에는 401만 봤다).
/// 2. 같은 응답에 [`assert_no_leak`]을 걸면 패닉한다 → 이 하네스의 단정은 **실제 라우트
///    출력에서 누출을 잡는다**.
///
/// 2번을 위해 패닉 훅을 잠시 조용한 것으로 갈아끼운다 — 기대된 패닉의 백트레이스가 테스트
/// 출력을 어지럽히지 않게 하기 위함이다. 훅은 프로세스 전역이므로 [`ENV_LOCK`]이 이 구간을
/// 다른 테스트와 직렬화한다.
#[tokio::test]
async fn harness_catches_a_value_that_reaches_the_screen() {
    let _guard = ENV_LOCK.lock().await;
    let auth = auth_state_locked();
    plant_secrets();
    let config_path = write_config("canary");
    let ctx = ctx(&config_path);

    let route = x_backup::web::routes::config::CONFIG_PATH;
    let (status, headers, body) = fetch(&ctx, &auth, route).await;
    assert_rendered_page(route, status, &body);
    assert!(
        body.contains(CANARY_PROFILE),
        "카나리가 화면에 없다 — 이 하네스는 인증 뒤 본문을 보지 못하고 있다: {body}"
    );

    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        assert_no_leak(route, status, &headers, &body, CANARY_PROFILE);
    }));
    std::panic::set_hook(previous_hook);
    assert!(
        caught.is_err(),
        "화면에 그대로 렌더된 값을 assert_no_leak이 잡지 못했다 — 이 하네스는 죽어 있다"
    );

    std::env::remove_var(ENV_MONGO_URI);
    std::env::remove_var(ENV_S3_CREDS);
}

/// sanity: `known_routes()`의 각 항목이 실제로 라우터에 존재하는지(404가 아닌지)
/// 확인한다. 목록에 오타가 있거나 라우트가 삭제됐는데 목록만 남아 있으면, 그 항목은
/// 아무것도 검사하지 않는 죽은 엔트리가 되어 위 누출 테스트가 조용히 커버리지를
/// 잃는다 — 이 테스트가 그 드리프트를 잡는다.
///
/// 완전하지 않다: "라우트가 추가됐는데 목록에 아예 안 실림"은 이 테스트로 못 잡는다
/// (자동 열거가 불가능한 이유는 파일 헤더 참고) — 그건 리뷰/컨벤션의 몫이다.
#[tokio::test]
async fn known_routes_are_registered() {
    let _guard = ENV_LOCK.lock().await;
    let auth = auth_state_locked();
    plant_secrets();
    let config_path = write_config("routes");
    let ctx = ctx(&config_path);
    for route in known_routes() {
        let (status, _, _) = fetch(&ctx, &auth, &route).await;
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{route}가 라우터에 없다 — known_routes()가 오래됐거나 오타"
        );
    }

    std::env::remove_var(ENV_MONGO_URI);
    std::env::remove_var(ENV_S3_CREDS);
}
