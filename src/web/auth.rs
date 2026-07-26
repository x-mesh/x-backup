//! 인증 계층 — fail-closed 토큰 인증 + age 개인키 권한 검사.
//!
//! ## 이 파일이 지키는 것
//! [`crate::web`] 모듈 헤더가 이미 못박았듯 이 서버는 age 복호화 개인키·프로덕션 DB 접속
//! 정보·config 쓰기 권한을 전부 쥔다. 열린 포트 하나가 전 백업 복호화 + 프로덕션 덮어쓰기
//! 경로이므로, 인증이 없는 채로 뜨는 경로가 하나라도 있으면 그 순간 이 계층의 존재 이유가
//! 없어진다. 그래서 [`AuthState::load_from_env`]는 **철저히 fail-closed**다 — 토큰을 못
//! 찾으면 서버가 아예 기동하지 않는다(조용한 무인증 기동은 설계상 불가능하다).
//!
//! ## 왜 세션 쿠키인가 (Bearer 헤더를 고르지 않은 이유)
//! [`crate::web::view`] 모듈 헤더가 명시하듯 이 콘솔은 아직 클라이언트 JS가 전혀 없는 순수
//! SSR(maud)이다. Bearer 헤더는 매 요청마다 `Authorization` 헤더를 코드로 실어야 하므로,
//! JS 없이 `<a href>`/폼 네비게이션만으로 화면을 오가는 사람은 애초에 Bearer를 실을 방법이
//! 없다. 그래서 브라우저가 요청마다 자동으로 실어주는 **세션 쿠키**를 골랐다 — 사람이 이
//! 콘솔을 쓴다는 전제(태스크 지침)와 "JS 없음"이라는 현재 상태를 동시에 만족하는 유일한
//! 선택지다.
//!
//! ## 쿠키 값은 토큰이 아니다 — 불투명 세션 ID다
//! 쿠키에 실리는 것은 [`AuthState::issue_session`]이 CSPRNG로 만든 256비트 세션 ID이고,
//! 설정된 토큰은 **로그인 폼을 통과하는 순간 말고는 어디에도 나가지 않는다.**
//! [`require_auth`]도 토큰이 아니라 세션을 본다 — 토큰을 쿠키에 넣어 보내면 통과하지
//! 못한다(`the_master_token_is_not_accepted_as_a_session_cookie` 테스트가 고정).
//!
//! ### 왜 이 구조여야 하는가 — 쿠키 스코프는 **포트를 구분하지 않는다**
//! 브라우저 쿠키의 스코프는 도메인 + 경로이며 **포트는 스코프에 들어가지 않는다**
//! (RFC 6265 §8.5). 실제 Chrome으로 확인했다 — `127.0.0.1:A`가 심은 쿠키를 같은 호스트의
//! `127.0.0.1:B`가 요청 헤더로 그대로 받는다. 결과가 둘이다:
//!
//! 1. **같은 호스트명의 다른 포트에서 도는 어떤 HTTP 서비스에도 브라우저가 `xb_session`을
//!    함께 보낸다.** `--allow-remote`로 `server.internal:8787`에 열어 두면 같은 호스트의
//!    `:3000` 사내 앱도 이 쿠키를 받는다. 루프백도 예외가 아니다.
//! 2. **`SameSite=Strict`가 그 포트를 막지 못한다.** same-site 판정은 "site"(등록 가능
//!    도메인) 단위이고 포트를 보지 않으므로, `:3000`에서 온 요청은 이 콘솔 입장에서
//!    same-site다 — 아래 "쿠키 속성 근거"가 CSRF 방어의 핵심 축으로 꼽은
//!    `SameSite=Strict`가 **정확히 이 경로에서만 뚫린다.**
//!
//! 이전 판본은 쿠키 값이 **토큰 그 자체**였다(CSPRNG를 직접 의존으로 올리지 않으려는
//! 선택이었다). 그래서 1번이 단순한 정보 노출이 아니라 **자격 유출**이었다 — 그 사내 앱의
//! 접근 로그 한 줄이 전 백업 복호화 + 프로덕션 덮어쓰기 권한이었고, 유출을 알아차려도
//! 되돌릴 방법이 토큰 회전 + 재기동뿐이었다.
//!
//! 지금은 두 결과를 각각 다른 수단으로 좁힌다:
//! - **1번**: 새는 값이 마스터 자격이 아니다. 세션은 [`SESSION_TTL`] 뒤 스스로 죽고,
//!   서버가 목록을 들고 있으므로 회수도 가능하다.
//! - **2번**: [`verify_same_origin`]이 상태 변경 요청의 출처를 **포트까지 포함해**
//!   검사한다(그 함수 doc에 "왜 CSRF 토큰이 아닌가"까지 적었다).
//!
//! 남는 위험은 "그 세션이 살아 있는 동안 콘솔을 쓸 수 있다"는 것이고, 원격 노출 배치에서는
//! 로그인 화면과 로그가 그 사실을 명시적으로 알린다([`TransportWarning::PlaintextRemote`]).
//!
//! ### 토큰 회전은 여전히 전체 로그아웃이다
//! 세션은 프로세스 메모리에만 있으므로 재기동하면 전부 사라진다. 즉 "토큰 재설정 + 재기동"이
//! 곧 강제 전체 로그아웃이라는 이전 설계의 성질은 그대로 유지된다 — 잃은 것이 없다.
//!
//! ## 쿠키 속성 근거
//! - `HttpOnly`: XSS로 `document.cookie`를 읽어 세션 ID를 훔치는 경로를 막는다.
//! - `SameSite=Strict`: CSRF 방어의 핵심 축이다 — 다른 사이트에서 이 콘솔로 보내는 요청에는
//!   쿠키가 아예 실리지 않으므로, 별도 CSRF 토큰 없이도 상태 변경 요청(로그인 포함)이
//!   위조되지 않는다.
//! - `Secure`: **바인딩 주소가 아니라 요청 스킴으로 판정한다**([`cookie_secure`]). 이전
//!   판본은 "루프백 밖에 열었다는 것은 앞단에 TLS 프록시가 있다는 전제"라고 적었지만,
//!   그 전제는 **실제 권장 배치와 방향이 반대다** — 프록시를 두면 오히려
//!   `프록시(443) → 127.0.0.1:8787`처럼 **루프백에** 바인딩한다. 그래서 바인딩 기준 판정은
//!   TLS가 있는 배치에서 정확히 `Secure`를 끄고(브라우저는 https인데 쿠키에 `Secure`가 없어
//!   평문 http 요청 하나에 마스터 토큰이 실려 나간다), TLS가 없는 원격 노출에서 정확히
//!   켜서(브라우저가 쿠키를 버려 로그인이 303 → `/` → 401로 무한 반복) 두 배치를 동시에
//!   망가뜨렸다. 둘 다 리뷰에서 실측됐다.
//!
//! ### `X-Forwarded-Proto`를 신뢰하는 근거 — 위조의 두 방향이 비대칭이다
//! 이 헤더는 클라이언트가 임의로 붙일 수 있다. 그런데 이 판정에서 거짓말의 결과가 방향에
//! 따라 완전히 다르다:
//!
//! - **없는 https를 있다고 위조** → 쿠키에 `Secure`가 붙는다 → 브라우저가 평문 연결에서
//!   그 쿠키를 저장하지 않는다. 위조한 그 클라이언트 **자신만** 로그인에 실패한다. 남의
//!   세션도, 서버 상태도 영향받지 않는다(권한 상승 경로가 아니다).
//! - **있는 https를 없다고 제거** → `Secure`가 빠진다 = 옛 판정의 나쁜 쪽과 같은 상태.
//!   그리고 이 방향을 할 수 있는 것은 앞단 프록시나 경로상의 중간자뿐이며, 그 위치를 잡은
//!   상대는 이미 평문 구간을 관측할 수 있다(이 헤더가 새로 만드는 위험이 아니다).
//!
//! 즉 이 헤더를 믿어서 **새로 생기는** 공격 표면이 없다. 그래도 추정에 기대고 싶지 않은
//! 배치를 위해 [`ENV_WEB_COOKIE_SECURE`]가 판정을 완전히 대체한다(`always`/`never`).
//! 프록시가 이 헤더를 지우거나 덧붙이는 정책은 프록시 설정의 몫이고, 그것이 우리 판정의
//! 신뢰 근거가 아니라는 점이 이 설계의 요점이다.
//!
//! ## 브루트포스 완화 — 세 겹이고, 각각 막는 것이 다르다
//! [`AuthState::verify`] 하나가 `POST /login` 폼 제출과 [`require_auth`] 미들웨어의 쿠키
//! 검사를 **둘 다** 처리한다. 이유: 공격자가 `/login`을 거치지 않고 `Cookie: xb_session=...`
//! 헤더를 직접 위조해 보호된 라우트를 반복 요청하면, `/login`에만 지연을 걸어도 그 경로는
//! 무제한으로 시도할 수 있는 우회가 된다.
//!
//! 이전 판본은 여기에 **지연 하나만** 두고 "이 우회를 구조적으로 막는다"고 단정했다. 그
//! 단정은 사실이 아니었고, 리뷰가 세 구멍을 전부 실측으로 재현했다. 아래 세 겹이 그
//! 각각에 대응한다:
//!
//! 1. **토큰 자체의 하한**([`MIN_TOKEN_LEN`]·[`MIN_TOKEN_ENTROPY_BITS`]). 지연과 상한이
//!    아무리 좋아도 토큰이 `a` 한 글자면 첫 시도에 뚫린다(실측: `XB_WEB_TOKEN=a`로 기동 →
//!    `Cookie: xb_session=a`로 `GET /` 200). 자격증명의 강도는 다른 어떤 완화 장치로도
//!    대신할 수 없는 축이므로 기동 시점에 fail-closed로 거부한다.
//! 2. **감쇠하는 실패 카운터**([`FAILURE_DECAY_PERIOD`]). 이전에는 검증 성공이 카운터를
//!    0으로 되돌렸다. [`require_auth`]가 **매 요청** `verify`를 부르므로, 운영자가 콘솔을
//!    쓰는 동안(특히 SSE 재연결) 공격자의 누적 지연이 계속 0으로 리셋됐다(실측: 실패 10회
//!    연속 7.24초 → 그 사이에 정상 요청을 하나씩 끼우면 1.83초). 지금은 **성공이 카운터를
//!    건드리지 않는다.** 대신 시간으로 감쇠한다 — 사람의 오타 몇 번은 [`FAILURE_DECAY_PERIOD`]
//!    마다 한 칸씩 잊히고, 초당 수십 번 두드리는 쪽은 감쇠보다 훨씬 빠르게 쌓여 상한에 붙는다.
//! 3. **동시 시도 상한**([`MAX_CONCURRENT_ATTEMPTS`]). 요청마다 자기 `sleep`을 await하는
//!    지연은 **병렬 공격에 공짜다**(실측: 동시 100회 실패 = 2.03초 ≈ 초당 49회, 순차 20회는
//!    27.66초). 지금은 실패한 시도가 지연을 소화하는 동안 세마포어 허가를 들고 있으므로
//!    실패 처리량이 `허가 수 ÷ 지연`으로 묶인다(상한 지연에서 초당 2회). 허가를 못 얻은
//!    시도는 **거부되지 않고 줄을 선다** — 거부(shed)해 버리면 공격자가 허가를 점유해
//!    정상 로그인을 막는 길이 새로 열린다.
//!
//!    이 상한이 정상 사용을 막지 않는 이유: **토큰이 맞는 검증은 세마포어를 지나지 않는다.**
//!    비교는 상수시간 몇 마이크로초이고, 허가는 그 뒤 "실패한 시도가 지연을 소화하는"
//!    구간에서만 잡는다. 운영자의 쿠키는 항상 맞으므로 공격이 진행되는 동안에도 콘솔은
//!    정상 속도로 동작한다.
//!
//! 남은 한계: 이 세 겹은 전부 **프로세스 전역**이고 IP별이 아니다. IP별 계정화는 신뢰할 수
//! 있는 클라이언트 주소(프록시 뒤에서는 `X-Forwarded-For`)를 전제하는데, 그 헤더는 위조
//! 방향이 위험한 쪽(공격자가 매 요청 다른 IP를 주장해 자기 계정을 초기화)이라 지금은 넣지
//! 않았다. 전역 상한은 공격자가 정상 사용자를 밀어내지 못하는 형태(성공 경로는 상한 밖)로
//! 골랐으므로, 전역이라는 성질이 곧 서비스 거부로 이어지지는 않는다.
//!
//! 다만 **쿠키가 아예 없는 요청**(로그인 안 한 평범한 방문)은 카운터를 올리지 않는다 —
//! `verify`를 부르지 않고 즉시 401로 끊는다. 그러지 않으면 정상 트래픽만으로 카운터가
//! 계속 올라가 지연이 상시 발동하는, 콘솔이 스스로에게 거는 DoS가 된다.
//!
//! ## age 개인키 권한 검사
//! [`check_age_identity_permissions`]는 `XB_AGE_IDENTITY_FILE`이 가리키는 파일을 **열지
//! 않는다** — 권한만 본다. 실제 복호화는 이 웹 프로세스가 아니라 나중에 뜨는 자식 프로세스가
//! 한다([`crate::web`] 모듈 헤더의 최상위 불변식). 권한 검사에 [`std::fs::metadata`]를 쓰는
//! 것이 핵심 함정 회피다 — `symlink_metadata`(=`lstat`)가 아니라 `metadata`(=`stat`)이므로
//! symlink를 자동으로 따라가 **가리키는 실제 파일**의 권한을 본다. symlink 자체 권한(대개
//! 0777로 보이는 관례값)을 검사하면 "symlink는 0600, 대상은 0644"인 우회를 놓친다.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Extension, Request, State};
use axum::http::{header, HeaderMap, StatusCode, Uri};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use maud::{html, Markup};
use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::web::view::components::{self, Level};
use crate::web::ServeConfig;

/// 토큰을 직접 담는 env(값 자체가 토큰).
pub const ENV_WEB_TOKEN: &str = "XB_WEB_TOKEN";
/// 토큰이 담긴 0600 파일의 경로를 담는 env(`ENV_WEB_TOKEN`과 배타적).
pub const ENV_WEB_TOKEN_FILE: &str = "XB_WEB_TOKEN_FILE";
/// 세션 쿠키의 `Secure` 판정을 못박는 env — `auto`(기본)·`always`·`never`.
///
/// 기본값 `auto`는 요청 스킴(`X-Forwarded-Proto`)을 보고 판정한다(모듈 헤더). 프록시가 그
/// 헤더를 전달하지 않는 배치는 `always`로 못박고, TLS 없이 원격 노출한 배치(권장하지 않지만
/// 실제로 존재한다)는 `never`로 못박아 로그인 무한 반복을 피한다.
pub const ENV_WEB_COOKIE_SECURE: &str = "XB_WEB_COOKIE_SECURE";
/// 프록시가 브라우저-프록시 구간의 스킴을 알려주는 관용 헤더.
const HEADER_FORWARDED_PROTO: &str = "x-forwarded-proto";
/// 세션 쿠키 이름.
pub const SESSION_COOKIE_NAME: &str = "xb_session";
/// 로그인 화면 경로. 라우터와 리다이렉트 대상이 이 상수를 공유한다.
pub const LOGIN_PATH: &str = "/login";

/// 세션 ID의 바이트 수 — 256비트.
///
/// 이 값은 "추측당하지 않을 폭"이 아니라 **넉넉히 남는 폭**으로 고른다. 세션 ID는
/// 온라인으로만 시험할 수 있고(오프라인 크래킹 대상이 아니다) 서버가 유효 세션 몇 개만
/// 들고 있으므로, 128비트로도 물리적으로 도달 불가능하다. 그럼에도 32바이트를 쓰는 이유는
/// 이 값을 아끼는 데서 얻는 것이 없기 때문이다 — hex로 64자, 쿠키 한도(4KB) 근처도 못 간다.
const SESSION_ID_BYTES: usize = 32;

/// 세션 하나의 **절대** 수명. 요청이 있을 때마다 갱신되는 유휴(idle) 방식이 아니다.
///
/// 절대 수명을 고른 이유: 갱신형이면 탈취된 세션이 공격자의 주기적 요청만으로 영원히
/// 살아남는다 — 그건 이 태스크가 없애려는 성질(회수 불가능한 자격)을 다시 만드는 것이다.
///
/// 12시간은 운영 근무 한 텀을 덮는 길이다. 장애 대응 중에 콘솔이 로그아웃되는 것은 그
/// 자체로 사고 대응을 방해하므로 짧게 잡지 않고, 하루를 넘기지도 않아 "어제 카페에서
/// 열어 둔 탭"이 오늘까지 유효하지는 않다.
const SESSION_TTL: Duration = Duration::from_secs(12 * 60 * 60);

/// 동시에 살아 있을 수 있는 세션 수 상한.
///
/// 이 콘솔은 공유 토큰 하나를 쓰므로 "사용자 수"가 아니라 **브라우저 수**가 세션 수다 —
/// 운영자 몇 명이 각자 노트북·데스크톱에서 열어도 열 몇 개를 넘지 않는다. 상한을 두는
/// 이유는 로그인을 반복해 메모리를 늘리는 경로를 닫기 위해서이고, 넘치면 **가장 오래된
/// 세션부터** 밀어낸다(가장 최근 로그인이 살아남는 쪽이 사람의 기대에 맞는다).
const MAX_SESSIONS: usize = 32;

/// 테스트 전용 — `XB_WEB_TOKEN`/`XB_WEB_TOKEN_FILE`/`XB_AGE_IDENTITY_FILE`을 만지는 테스트를
/// 이 파일과 `server.rs` 양쪽에서 직렬화한다. process-wide env는 병렬 테스트 간 공유
/// 상태라 동시 set/remove가 서로 간섭할 수 있다(`pipeline::stage`의 `ENV_GUARD`와 같은
/// 패턴 — 파일 경계를 넘어 같이 쓰라고 `pub(crate)`로 둔다).
#[cfg(test)]
pub(crate) static ENV_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 이 문턱 미만의 연속 실패는 지연 없이 즉시 401을 돌려준다(정상적인 오타 한 번 정도는
/// 브루트포스가 아니다). 이후부터 [`failure_delay`]가 지연을 끼운다.
const FAILURE_THRESHOLD: u32 = 3;
/// 문턱을 넘은 뒤 매 실패마다 늘어나는 지연 단위.
const BASE_DELAY: Duration = Duration::from_millis(200);
/// 지연의 상한 — 실패가 아무리 쌓여도 응답을 영원히 붙잡아두지 않는다(그 자체가 자원 고갈
/// 공격의 표적이 될 수 있다).
const MAX_DELAY: Duration = Duration::from_secs(2);

/// 실패 카운터가 한 칸 감쇠하는 데 걸리는 시간.
///
/// ## 60초의 근거
/// 성공으로 리셋하지 않기로 했으므로(모듈 헤더 2번), 카운터를 내려주는 유일한 힘이 시간이다.
/// 그 속도는 두 가지를 동시에 만족해야 한다:
///
/// - **사람의 오타는 잊혀야 한다.** 토큰을 잘못 붙여넣어 3~5번 실패한 운영자가, 몇 분 뒤
///   다시 왔을 때 상한 지연(2초)을 맞는 것은 방어가 아니라 벌이다. 60초면 5분 안에 5칸이
///   빠져 문턱 아래로 내려간다.
/// - **공격은 잊혀선 안 된다.** 동시 상한 아래에서 공격자가 낼 수 있는 최대 처리량은
///   초당 몇 회다 — 감쇠(분당 1칸)보다 두 자릿수 빠르게 쌓이므로 카운터는 곧바로 상한에
///   붙어 있고, 감쇠가 공격에 주는 이득은 사실상 없다.
///
/// 더 짧게(예: 5초) 잡으면 공격자가 잠깐 쉬는 것만으로 지연을 초기화할 수 있고, 더 길게
/// (예: 1시간) 잡으면 오타 몇 번이 근무 시간 내내 남는다.
const FAILURE_DECAY_PERIOD: Duration = Duration::from_secs(60);

/// **실패한** 인증 시도가 지연을 소화하는 동안 동시에 존재할 수 있는 최대 개수.
///
/// ## 4의 근거
/// 이 값이 곧 실패 처리량의 상한이다 — `허가 수 ÷ 지연`. 상한 지연(2초)에서 초당 2회,
/// 문턱 직후(200ms)에서 초당 20회다. 실측된 병렬 공격(초당 49회, 커넥션 1000개면 ~500회)
/// 대비 두 자릿수를 깎는다.
///
/// 1(완전 직렬화)로 두지 않은 이유: 사람 여럿이 같은 콘솔을 쓰는 배치에서 한 명의 오타가
/// 다른 사람의 로그인 시도를 통째로 줄 세우는 것은 과하다. 4는 "동시에 로그인 폼을 두드리는
/// 사람이 몇 명 있어도 서로를 크게 방해하지 않는" 하한이면서, 공격 처리량을 유의미하게
/// 묶는 값이다. 성공하는 검증은 이 상한을 아예 지나지 않으므로(모듈 헤더 3번) 이 값을
/// 작게 잡는 비용은 실패한 시도에만 붙는다.
const MAX_CONCURRENT_ATTEMPTS: usize = 4;

/// 웹 콘솔 토큰의 최소 길이(바이트).
///
/// ## 24의 근거 — 마스터 자격증명은 마스킹 하한과 다른 축이다
/// [`crate::web::mask::MIN_SECRET_LEN`]은 8인데 그 8은 **보안 강도가 아니라 오탐 방지**
/// 근거다("짧은 값을 마스킹 대상으로 등록하면 무해한 텍스트가 우연히 일치해 뭉개진다").
/// 이 토큰은 반대로 **강도 자체가 목적**이다 — 이 하나가 age 개인키·프로덕션 접속 정보·
/// config 쓰기 권한 전부를 여는 유일한 문이다.
///
/// 24바이트는 흔한 생성 명령이 그대로 내놓는 길이다: `openssl rand -hex 12`(24자, 96비트),
/// `openssl rand -base64 18`(24자, 144비트), `head -c 18 /dev/urandom | base64`. 즉 하한을
/// 24로 잡아도 운영자가 "특별한 방법"을 찾을 필요가 없다. 그리고 96비트는 이 문의 다른
/// 방어(동시 상한 아래 초당 몇 회)와 곱하면 우주 나이를 넘는 시간이 되므로, 길이 축에서
/// 더 요구할 실익이 없다.
///
/// 반대로 이보다 낮추면 안 되는 이유: 16자 랜덤 hex(64비트)는 오프라인 공격에는 여전히
/// 충분하지만, 사람이 "16자면 됐다"고 생각할 때 실제로 쓰는 것은 랜덤이 아니라 사전 단어의
/// 조합이다. 하한을 넉넉히 잡는 것이 그 습관을 막는 가장 싼 방법이다.
pub const MIN_TOKEN_LEN: usize = 24;

/// 토큰의 문자 분포로 추정한 최소 엔트로피(비트) — [`estimated_entropy_bits`].
///
/// ## 64비트의 근거
/// 길이 하한만 두면 `aaaaaaaaaaaaaaaaaaaaaaaa`(24자)가 통과한다. 이 하한은 **문자 종류가
/// 너무 적은** 토큰을 걸러낸다: 한 글자 반복은 0비트, 두 글자 교대는 24비트, 세 글자
/// 조합도 40비트 아래다. 반대편 여유도 충분하다 — 랜덤 24자 hex는 약 91비트, base64는
/// 약 105비트다. 64는 그 사이에서, 실제 난수 생성기의 출력을 오탐으로 거부할 확률이
/// 사실상 0인 자리다(오탐의 대가는 "콘솔이 기동하지 않음"이라 싸지 않다).
///
/// ## 이 하한이 **못** 잡는 것과, 그래서 함께 두는 검사
/// 이 값은 문자 빈도만 본다 — 배열 순서를 모른다. 그래서
/// `passwordpasswordpassword`(문자 7종, 약 66비트)는 이 하한을 통과한다. 그 부류는
/// [`has_repeated_period`]가 "짧은 패턴의 반복"이라는 **구조**로 따로 거부한다. 두 검사가
/// 보는 축이 다르므로 둘 다 필요하다.
///
/// **그럼에도 이것은 진짜 엔트로피가 아니다.** 예측 가능한 생성기가 만든 골고루 섞인
/// 문자열(예: 오늘 날짜의 md5)이나 사전 단어의 조합(`correcthorsebatterystaple`)은 통과한다 —
/// 그걸 잡으려면 사전과 생성기 모델이 필요하고, 그건 새 의존성이다. 이 검사가 하는 일은
/// "명백히 퇴화된 것을 기동 시점에 거부"까지이고 그 이상을 주장하지 않는다(이 파일의 이전
/// 판본이 지연 하나로 "이 우회를 구조적으로 막는다"고 단정했던 실수를 반복하지 않기 위해
/// 여기 명시한다).
pub const MIN_TOKEN_ENTROPY_BITS: f64 = 64.0;

/// 실패 카운터와 마지막 실패 시각. [`AuthState::note_failure`]만 이 값을 바꾼다.
///
/// 시각을 [`tokio::time::Instant`]로 들고 있는 이유: 감쇠를 검증하는 테스트가 실제로 몇
/// 분을 자지 않고 `tokio::time::advance`로 가상 시계를 밀어 확인할 수 있다(같은 시계를
/// [`tokio::time::sleep`]도 쓴다).
#[derive(Debug, Default)]
struct FailureWindow {
    /// 감쇠를 반영한 누적 실패 수.
    failures: u32,
    /// 마지막 실패 시각(없으면 아직 실패가 없다).
    last_failure: Option<Instant>,
}

/// 발급된 세션 하나.
///
/// 값(`id`)은 브라우저에만 있는 것이 아니라 **여기에도** 있다 — 그래서 세션은 회수할 수
/// 있다. 마스터 토큰을 쿠키에 그대로 싣던 이전 설계에는 이 자리가 없었고, 그래서 "이
/// 쿠키만 무효화한다"가 원리적으로 불가능했다(모듈 헤더 "쿠키 값은 토큰이 아니다").
struct Session {
    /// 세션 ID 원본 바이트. 쿠키에는 이 값을 hex로 실어 보낸다.
    id: [u8; SESSION_ID_BYTES],
    /// 발급 시각 — [`SESSION_TTL`] 만료 판정의 기준.
    issued: Instant,
}

/// 인증 상태 — 프로세스 메모리에서만 산다. 재시작하면 실패 카운터도 초기화된다(의도된
/// 동작 — 영속시킬 만큼 중요한 상태가 아니고, 재시작은 토큰 회전과 함께 일어난다).
///
/// `Debug`는 아래에서 손으로 구현한다(derive 대신) — 테스트의 `expect_err` 등이 `T: Debug`를
/// 요구하지만, 기본 파생은 `token` 필드를 그대로 찍으므로 토큰이 패닉 메시지·로그에 샐 수 있다.
pub struct AuthState {
    /// 설정된 토큰의 바이트 표현(비교는 항상 [`constant_time_eq`]를 거친다).
    token: Vec<u8>,
    /// 실패 이력 — **성공은 이 값을 건드리지 않는다**(모듈 헤더 2번). 감쇠만 내려준다.
    ///
    /// `std::sync::Mutex`인 이유: 이 잠금은 산술 몇 줄만 감싸고 `.await`를 넘지 않는다
    /// (지연 sleep은 가드를 놓은 뒤에 한다 — [`AuthState::verify`]). 두 값(카운터·시각)을
    /// 원자적으로 함께 갱신해야 하므로 원자 정수 두 개로는 표현할 수 없다.
    failures: Mutex<FailureWindow>,
    /// 실패한 시도가 지연을 소화하는 구간의 동시 진입 상한(모듈 헤더 3번).
    ///
    /// 성공하는 검증은 이 세마포어를 지나지 않는다 — 그래서 공격이 허가를 다 점유해도
    /// 운영자의 정상 요청은 영향받지 않는다.
    attempt_slots: Semaphore,
    /// 살아 있는 세션들. 오래된 것이 앞에 온다(발급 순서대로 push하고 만료만 걷어낸다).
    ///
    /// `std::sync::Mutex`인 이유는 [`AuthState::failures`]와 같다 — 이 잠금은 짧은 벡터
    /// 조작만 감싸고 `.await`를 넘지 않는다.
    sessions: Mutex<Vec<Session>>,
}

impl std::fmt::Debug for AuthState {
    /// 토큰 바이트를 절대 찍지 않는다 — `expect_err`(테스트) 등이 `T: Debug`를 요구해서
    /// derive 대신 손으로 구현하고, `token` 필드는 길이만 남긴다(태스크 지침: 토큰 내용이
    /// 로그·에러 메시지에 절대 나오지 않게 하라).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let failures = self
            .failures
            .lock()
            .map(|w| w.failures)
            .unwrap_or_else(|e| e.into_inner().failures);
        f.debug_struct("AuthState")
            .field("token", &"[REDACTED]")
            .field("failures", &failures)
            .field(
                "free_attempt_slots",
                &self.attempt_slots.available_permits(),
            )
            .finish()
    }
}

impl AuthState {
    /// env에서 인증 설정을 fail-closed로 로드한다.
    ///
    /// `ENV_WEB_TOKEN`(직접 값) / `ENV_WEB_TOKEN_FILE`(0600 파일 경로) 중 **정확히 하나만**
    /// 요구한다 — 어느 쪽이 우선인지 암묵적으로 정하지 않는다. `--bind` 파싱이 호스트명을
    /// 거부하고 명시적 IP만 받는 것과 같은 원칙("추측하지 않는다", [`crate::web`] 모듈 헤더).
    pub fn load_from_env() -> Result<Self> {
        let direct = std::env::var(ENV_WEB_TOKEN).ok();
        let from_file = std::env::var(ENV_WEB_TOKEN_FILE).ok();

        let raw_token = match (direct, from_file) {
            (None, None) => {
                return Err(XBackupError::Config(format!(
                    "웹 콘솔 인증이 설정되지 않았습니다 — {ENV_WEB_TOKEN}(토큰 직접 지정) 또는 \
                     {ENV_WEB_TOKEN_FILE}(0600 파일 경로) 중 하나를 설정해야 `serve`가 기동합니다. \
                     인증 없이 뜨는 경로는 없습니다(fail-closed) — 이 서버는 복호화 개인키와 \
                     프로덕션 접속 정보를 상시 보유합니다."
                )));
            }
            (Some(_), Some(_)) => {
                return Err(XBackupError::Config(format!(
                    "{ENV_WEB_TOKEN}과 {ENV_WEB_TOKEN_FILE}이 동시에 설정되어 있습니다 — \
                     어느 쪽이 우선인지 추측하지 않습니다. 하나만 설정하세요."
                )));
            }
            (Some(v), None) => v,
            (None, Some(path)) => {
                ensure_secret_file_permissions(
                    Path::new(&path),
                    "웹 콘솔 토큰",
                    ENV_WEB_TOKEN_FILE,
                )?;
                std::fs::read_to_string(&path).map_err(|e| {
                    XBackupError::Config(format!(
                        "{ENV_WEB_TOKEN_FILE} 파일을 읽을 수 없습니다({path}): {e}"
                    ))
                })?
            }
        };

        let token = raw_token.trim();
        if token.is_empty() {
            return Err(XBackupError::Config(format!(
                "웹 콘솔 토큰이 비어 있습니다(공백만 있어도 거부) — {ENV_WEB_TOKEN} 또는 \
                 {ENV_WEB_TOKEN_FILE}에 실제 토큰 값을 넣으세요."
            )));
        }
        if let Some(bad) = token.chars().find(|c| !is_cookie_safe_char(*c)) {
            return Err(XBackupError::Config(format!(
                "웹 콘솔 토큰에 쿠키 값으로 옮길 수 없는 문자 '{bad}'가 있습니다 — 이 토큰은 \
                 인코딩 없이 세션 쿠키 값으로 그대로 쓰입니다. 공백·세미콜론·쉼표·따옴표·백슬래시·\
                 제어문자를 뺀 값을 쓰세요(영문·숫자·기호 대부분은 안전합니다)."
            )));
        }
        ensure_token_strength(token)?;

        Ok(Self::with_token(token))
    }

    /// 검증이 끝난 토큰으로 상태를 만든다 — 공개 경로는 [`AuthState::load_from_env`] 하나다.
    fn with_token(token: &str) -> Self {
        Self {
            token: token.as_bytes().to_vec(),
            failures: Mutex::new(FailureWindow::default()),
            attempt_slots: Semaphore::new(MAX_CONCURRENT_ATTEMPTS),
            sessions: Mutex::new(Vec::new()),
        }
    }

    /// 테스트 전용 생성자 — env를 거치지 않고 임의 토큰으로 상태를 만든다.
    ///
    /// **강도 검사([`ensure_token_strength`])를 일부러 통과하지 않는다.** 이 생성자를 쓰는
    /// 테스트(여러 파일의 라우트 테스트)는 인증 로직이 아니라 화면·라우팅을 보는 것이고,
    /// 그 테스트들이 24자 토큰을 들고 다녀야 할 이유가 없다. 강도 검사가 실제로 걸리는지는
    /// env를 거치는 경로(`load_from_env`)를 직접 두드리는 아래 테스트들이 확인한다.
    #[cfg(test)]
    pub(crate) fn for_test(token: &str) -> Arc<Self> {
        Arc::new(Self::with_token(token))
    }

    /// 테스트 전용 — 상태와 **그 상태에서 유효한 세션 ID**를 함께 만든다.
    ///
    /// 라우트 테스트는 "인증을 통과한 요청"을 만들어야 하는데, 세션 도입 뒤로는 그것이
    /// 토큰 문자열을 쿠키에 넣는 것으로는 안 된다(그게 바로 고친 취약점이다). 세션은
    /// **발급한 그 인스턴스에만** 있으므로, 미들웨어와 쿠키가 같은 `AuthState`를 봐야 한다 —
    /// 이 생성자가 둘을 한 번에 돌려주는 이유다(따로 만들면 조용히 401이 난다).
    #[cfg(test)]
    pub(crate) fn for_test_with_session(token: &str) -> (Arc<Self>, String) {
        let state = Arc::new(Self::with_token(token));
        let session = state.issue_session().expect("테스트 세션 발급 실패");
        (state, session)
    }

    /// 자격 증명을 상수시간으로 검증한다.
    ///
    /// `POST /login` 폼 제출과 [`require_auth`]의 쿠키 검사가 **모두** 이 메서드를 거친다
    /// (모듈 헤더 "브루트포스 완화" 참조).
    ///
    /// ## 성공 경로가 아무 상태도 건드리지 않는 것이 요점이다
    /// 맞는 토큰은 비교만 하고 즉시 돌아간다 — 카운터를 리셋하지도 않고([`FailureWindow`]
    /// doc), 세마포어 허가를 잡지도 않는다. 그래서 (1) 운영자의 정상 사용이 공격자의 누적
    /// 지연을 지워 주지 않고, (2) 공격이 허가를 다 점유한 순간에도 운영자는 정상 속도로
    /// 콘솔을 쓴다.
    ///
    /// 실패 경로는 카운터를 올리고([`AuthState::note_failure`]가 감쇠까지 반영), 문턱
    /// ([`FAILURE_THRESHOLD`])을 넘으면 **허가를 든 채로** 지연을 소화한다.
    pub async fn verify(&self, candidate: &str) -> bool {
        if constant_time_eq(candidate.trim().as_bytes(), &self.token) {
            return true;
        }
        let delay = self.note_failure(Instant::now());
        if !delay.is_zero() {
            // 허가를 얻지 못한 시도는 **거부되지 않고 줄을 선다**(모듈 헤더 3번 — 거부하면
            // 공격자가 허가를 점유해 정상 로그인을 막는 길이 열린다). `acquire`는 세마포어를
            // 닫았을 때만 실패하고 우리는 닫지 않으므로, 실패 시에도 지연은 그대로 소화한다.
            let _permit = self.attempt_slots.acquire().await;
            tokio::time::sleep(delay).await;
        }
        false
    }

    /// 실패 하나를 기록하고 이번 응답에 끼울 지연을 돌려준다.
    ///
    /// 잠금은 이 함수 안에서 시작하고 끝난다 — 지연 sleep은 호출부가 가드를 놓은 뒤에 한다
    /// (동기 뮤텍스를 `.await` 너머로 들고 가면 `clippy::await_holding_lock`이고, 실제로도
    /// 한 요청의 sleep이 다른 요청의 카운터 갱신을 2초씩 막는다).
    ///
    /// 잠금 오염(poison)은 값을 그대로 꺼내 계속 쓴다 — 보호 대상이 정수 두 개뿐이라
    /// 이전 패닉이 남길 불변식 위반이 없고, 여기서 패닉을 전파하면 **인증 실패 하나가
    /// 콘솔 전체를 500으로 만든다**(`crate::web::job::env_guard`와 같은 판단).
    fn note_failure(&self, now: Instant) -> Duration {
        let mut window = self
            .failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let carried = match window.last_failure {
            Some(previous) => {
                decay_failures(window.failures, now.saturating_duration_since(previous))
            }
            None => 0,
        };
        window.failures = carried.saturating_add(1);
        window.last_failure = Some(now);
        failure_delay(window.failures)
    }

    /// 세션 토큰을 마스킹 레지스트리에 등록한다 — 값을 **밖으로 돌려주지 않는다.**
    ///
    /// 라우트가 자식 출력이나 오류 메시지를 그릴 때 이 토큰이 우연히 섞이는 경로를 막기
    /// 위한 2차 방어다([`crate::web::mask`]). 게터(`fn token(&self) -> &str`)를 두지 않고
    /// **레지스트리를 받아 스스로 넣는** 형태인 이유: 토큰을 꺼낼 수 있는 공개 경로가
    /// 생기면 그 값이 응답·로그로 흘러갈 새 표면이 열린다. 이 방향이면 호출부가 얻는 것은
    /// "그 값을 지우는 능력"뿐이다.
    ///
    /// `XB_WEB_TOKEN`(env)인지 `XB_WEB_TOKEN_FILE`(파일)인지와 무관하게 동작한다 — env를
    /// 다시 읽는 방식은 파일로 설정한 배포에서 조용히 아무것도 등록하지 못한다.
    ///
    /// 토큰이 `MIN_SECRET_LEN` 미만이면 레지스트리가 등록을 거부하는데(과잉 마스킹 방지),
    /// [`MIN_TOKEN_LEN`]이 그보다 크므로 정상 기동 경로에서는 항상 등록된다.
    pub fn register_session_secret(&self, registry: &mut crate::web::mask::SecretRegistry) {
        // 토큰은 로드 시점에 쿠키 안전 ASCII로 검증됐으므로 UTF-8 변환이 실패할 수 없다.
        if let Ok(token) = std::str::from_utf8(&self.token) {
            registry.register(token);
        }
    }

    /// 새 세션을 발급하고 **쿠키에 실을 값**(hex 문자열)을 돌려준다.
    ///
    /// 토큰 검증에 성공한 직후에만 부른다([`login_submit`]) — 이 함수 자체는 자격을 묻지
    /// 않는다. 그 분리가 요점이다: "누구인지 확인한다"(토큰)와 "그 확인을 이 브라우저에
    /// 얼마 동안 위임한다"(세션)는 서로 다른 결정이고, 후자만 쿠키로 나간다.
    ///
    /// 발급하면서 만료된 세션을 걷어내고 상한([`MAX_SESSIONS`])을 지킨다 — 별도의 청소
    /// 태스크를 두지 않는 이유는 이 저장소가 커지는 유일한 계기가 로그인이기 때문이다.
    /// 아무도 로그인하지 않으면 자랄 일도 없으므로 주기적으로 깨어날 이유가 없다.
    ///
    /// CSPRNG가 실패하면 **세션을 만들지 않고 그대로 전파한다.** 예측 가능한 세션 ID를
    /// 내주느니 로그인이 실패하는 편이 낫다(fail-closed — 이 파일의 다른 판단들과 같다).
    pub fn issue_session(&self) -> Result<String> {
        self.issue_session_at(Instant::now())
    }

    /// [`AuthState::issue_session`]의 본체 — 현재 시각을 인자로 받는다.
    ///
    /// [`AuthState::note_failure`]와 같은 이유로 시각을 주입한다: 만료 규칙을 테스트하려면
    /// 12시간을 기다리는 대신 시각을 옮길 수 있어야 한다.
    fn issue_session_at(&self, now: Instant) -> Result<String> {
        let mut id = [0u8; SESSION_ID_BYTES];
        getrandom::fill(&mut id).map_err(|e| {
            XBackupError::Failure(format!(
                "세션 ID를 만들 난수를 얻지 못했습니다: {e} — 예측 가능한 세션을 내주지 \
                 않기 위해 로그인을 중단합니다."
            ))
        })?;

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|s| now.saturating_duration_since(s.issued) < SESSION_TTL);
        while sessions.len() >= MAX_SESSIONS {
            // 앞이 가장 오래된 것 — push 순서가 곧 발급 순서이고 retain이 순서를 지킨다.
            sessions.remove(0);
        }
        sessions.push(Session { id, issued: now });
        Ok(hex_encode(&id))
    }

    /// 쿠키로 온 값이 살아 있는 세션인지 확인한다.
    ///
    /// ## 여기서 실패해도 브루트포스 카운터를 올리지 않는다
    /// [`AuthState::verify`](토큰)와 의도적으로 다르다. 세션 ID는 256비트 난수라 온라인
    /// 추측이 성립할 수 없으므로 지연으로 얻을 것이 없고, 반대로 카운터를 공유하면 **아무나
    /// 쓰레기 쿠키를 반복해 보내는 것만으로 운영자의 로그인을 느리게 만들 수 있다**. 즉
    /// 여기서 카운터를 올리는 설계는 방어가 아니라 새로운 공격 표면이다.
    ///
    /// 비교는 [`constant_time_eq`]로 하고 **일치를 찾아도 조기 반환하지 않는다** — 반환
    /// 시점이 "몇 번째 세션에서 맞았는가"를 흘리지 않게 한다. 세션 수가
    /// [`MAX_SESSIONS`]로 묶여 있어 전부 도는 비용이 무시할 수준이라 가능한 선택이다.
    pub fn session_is_valid(&self, candidate: &str) -> bool {
        self.session_is_valid_at(candidate, Instant::now())
    }

    /// [`AuthState::session_is_valid`]의 본체 — 현재 시각을 인자로 받는다(만료 테스트용).
    fn session_is_valid_at(&self, candidate: &str, now: Instant) -> bool {
        let Some(bytes) = hex_decode(candidate.trim()) else {
            return false;
        };

        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        sessions.retain(|s| now.saturating_duration_since(s.issued) < SESSION_TTL);

        let mut matched = false;
        for session in sessions.iter() {
            matched |= constant_time_eq(&bytes, &session.id);
        }
        matched
    }

    /// 살아 있는 세션 수 — 테스트·진단용.
    #[cfg(test)]
    fn session_count(&self) -> usize {
        self.sessions
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .len()
    }

    /// 현재(감쇠 반영 전) 누적 실패 수 — 테스트·진단용.
    #[cfg(test)]
    fn failure_count(&self) -> u32 {
        self.failures
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .failures
    }
}

/// 세션 ID 바이트를 소문자 hex로 적는다.
///
/// hex를 고른 이유는 **쿠키 값으로 그대로 안전하기 때문**이다 — base64는 `+`/`/`/`=`가
/// 섞여 인코딩 규칙을 한 겹 더 얹어야 하는데, 32바이트가 64자가 되는 정도는 아낄 가치가 없다.
fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write;
        // `String`에 쓰는 것은 실패할 수 없다.
        let _ = write!(out, "{b:02x}");
    }
    out
}

/// hex 문자열을 세션 ID 바이트로 되돌린다. 길이·문자가 정확히 맞지 않으면 `None`이다.
///
/// 길이를 먼저 못박는 것이 중요하다 — 짧은 입력이 통과해 [`constant_time_eq`]의 길이
/// 불일치 경로로 흘러가면, 그 함수가 하는 일은 같아도 이 자리에서 "형식이 틀렸다"와
/// "값이 틀렸다"가 뭉개진다.
fn hex_decode(text: &str) -> Option<[u8; SESSION_ID_BYTES]> {
    if text.len() != SESSION_ID_BYTES * 2 {
        return None;
    }
    let mut out = [0u8; SESSION_ID_BYTES];
    for (slot, pair) in out.iter_mut().zip(text.as_bytes().chunks_exact(2)) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        *slot = (hi * 16 + lo) as u8;
    }
    Some(out)
}

/// 토큰이 마스터 자격증명으로서 최소 강도를 갖는지 검사한다(fail-closed).
///
/// 근거는 [`MIN_TOKEN_LEN`]·[`MIN_TOKEN_ENTROPY_BITS`] doc에 있다. 메시지에는 **토큰 값을
/// 절대 담지 않는다** — 길이/판정만 말한다(길이는 이미 설정한 사람이 아는 값이고, 기동
/// 실패 메시지는 로그·터미널에 그대로 남는다).
fn ensure_token_strength(token: &str) -> Result<()> {
    if token.len() < MIN_TOKEN_LEN {
        return Err(XBackupError::Config(format!(
            "웹 콘솔 토큰이 너무 짧습니다({}바이트, 최소 {MIN_TOKEN_LEN}바이트) — 이 토큰 \
             하나가 복호화 개인키와 프로덕션 접속 정보, config 쓰기 권한 전부를 여는 문이므로 \
             기동을 거부합니다. `openssl rand -base64 18`(또는 `openssl rand -hex 12`)로 \
             만든 값을 쓰세요.",
            token.len()
        )));
    }
    let bits = estimated_entropy_bits(token);
    if bits < MIN_TOKEN_ENTROPY_BITS {
        return Err(XBackupError::Config(format!(
            "웹 콘솔 토큰이 너무 단조롭습니다(추정 엔트로피 {bits:.0}비트, 최소 \
             {MIN_TOKEN_ENTROPY_BITS:.0}비트) — 쓰인 문자 종류가 너무 적습니다. 길이만 \
             채운 토큰은 사실상 몇 번의 시도로 뚫립니다. `openssl rand -base64 18`처럼 \
             난수 생성기로 만든 값을 쓰세요."
        )));
    }
    if has_repeated_period(token) {
        return Err(XBackupError::Config(
            "웹 콘솔 토큰이 짧은 패턴의 반복입니다 — 길이를 채웠어도 실제 후보 공간은 그 \
             패턴 하나만큼입니다. `openssl rand -base64 18`처럼 난수 생성기로 만든 값을 \
             쓰세요."
                .to_string(),
        ));
    }
    Ok(())
}

/// 문자열 전체가 자기보다 짧은 패턴의 정수배 반복인지 — `"abcabcabc"` → `true`.
///
/// [`MIN_TOKEN_ENTROPY_BITS`]가 문자 **빈도**만 보기 때문에 필요한 짝 검사다(그쪽 doc 참조).
/// 난수로 만든 24자 문자열이 우연히 짧은 패턴의 정수배가 될 확률은 무시할 수 있으므로
/// (2회 반복이라도 앞 12자와 뒤 12자가 정확히 같아야 한다) 오탐 위험이 사실상 없다.
///
/// 바이트로 비교한다 — 대상은 [`is_cookie_safe_char`]를 통과한 ASCII뿐이다.
fn has_repeated_period(token: &str) -> bool {
    let bytes = token.as_bytes();
    let len = bytes.len();
    (1..=len / 2)
        .filter(|period| len.is_multiple_of(*period))
        .any(|period| bytes.chunks(period).all(|chunk| chunk == &bytes[..period]))
}

/// 문자열 자신의 문자 빈도로 엔트로피를 추정한다(섀넌 엔트로피 × 길이, 단위는 비트).
///
/// 바이트 단위로 센다 — 대상은 쿠키 안전 문자(ASCII)뿐이므로 바이트와 문자가 일치한다
/// ([`is_cookie_safe_char`]가 그보다 넓은 값을 이미 거부한다). **이 값이 진짜 엔트로피가
/// 아닌 이유**는 [`MIN_TOKEN_ENTROPY_BITS`] doc에 적었다.
fn estimated_entropy_bits(token: &str) -> f64 {
    let bytes = token.as_bytes();
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u32; 256];
    for byte in bytes {
        counts[*byte as usize] += 1;
    }
    let len = bytes.len() as f64;
    let per_char: f64 = counts
        .iter()
        .filter(|count| **count > 0)
        .map(|count| {
            let p = f64::from(*count) / len;
            -p * p.log2()
        })
        .sum();
    // `.max(0.0)`은 **음수 0을 없애기 위한 것**이다. 한 글자만 반복된 문자열은 p=1이라
    // `-1.0 * log2(1.0)` = `-0.0`이 되고, 그 값이 그대로 기동 거부 메시지에 "-0비트"로
    // 찍힌다(실서버에서 확인했다). 값의 의미는 같지만 운영자가 읽는 문장이 고장 나 보인다.
    (per_char * len).max(0.0)
}

/// 마지막 실패 이후 흐른 시간만큼 카운터를 깎는다 — [`FAILURE_DECAY_PERIOD`]마다 한 칸.
///
/// 순수 함수라 감쇠 정책을 시계 없이 검증할 수 있다. 포화 연산만 쓴다(음수·오버플로 없음).
fn decay_failures(failures: u32, elapsed: Duration) -> u32 {
    let period = FAILURE_DECAY_PERIOD.as_secs();
    // 상수가 0이 되면 나눗셈이 패닉한다 — 정의상 0으로 두지 않지만, 미래에 누가 값을 바꿔도
    // 런타임 패닉으로 번지지 않게 방어한다(그 경우 감쇠 없음 = 안전한 방향).
    if period == 0 {
        return failures;
    }
    let steps = elapsed.as_secs() / period;
    failures.saturating_sub(u32::try_from(steps).unwrap_or(u32::MAX))
}

/// 연속 실패 횟수에 따른 지연을 계산한다(순수 함수 — 실제 sleep은 호출부가 한다).
///
/// 문턱 미만은 지연이 없고, 문턱을 넘으면 [`BASE_DELAY`] 단위로 선형 증가하며
/// [`MAX_DELAY`]에서 잘린다. 지수 증가를 쓰지 않는 이유: 지연 자체도 무제한으로 늘면
/// 서버가 스스로 만든 자원(대기 중인 커넥션)을 소진시키는 표적이 될 수 있어, 상한을 두고
/// 성장도 완만하게 잡았다.
fn failure_delay(failures: u32) -> Duration {
    if failures < FAILURE_THRESHOLD {
        return Duration::ZERO;
    }
    let steps = failures - FAILURE_THRESHOLD + 1;
    BASE_DELAY.saturating_mul(steps).min(MAX_DELAY)
}

/// 상수시간 바이트 비교 — 토큰 검증에 쓴다.
///
/// 표준 `==`(슬라이스 비교)는 구현상 첫 불일치 바이트에서 조기 반환할 수 있어, 그 미세한
/// 시간 차이로 공격자가 바이트를 하나씩 추측하는 타이밍 사이드채널이 이론상 성립한다. 이
/// 함수는 XOR 누적으로 항상 두 슬라이스 전체를 순회하고 분기 없이 결과를 접는다.
///
/// 길이가 다르면 그 사실 자체를 밝히며 조기 반환한다 — 토큰 길이는 숨길 대상이 아니다
/// (설정한 사람이 이미 알고, 비교 결과의 참/거짓만이 진짜 비밀이다). 길이가 같을 때의
/// 바이트별 비교만 상수시간이면 충분하다.
///
/// 외부 crate(`subtle` 등)를 추가하지 않는다 — 검증 대상이 짧은 토큰 문자열 하나뿐이라
/// XOR 누적을 직접 구현하는 비용이 새 의존성을 추가하는 비용보다 낮다.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff: u8 = 0;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// RFC 6265 `cookie-octet` 허용 범위 — 웹 콘솔 토큰이 인코딩 없이 쿠키 값으로 그대로
/// 나가므로, 그 문법을 깨거나 파싱을 모호하게 만들 수 있는 문자(공백·세미콜론·쉼표·
/// 백슬래시·큰따옴표·제어문자)를 로드 시점에 거부한다.
fn is_cookie_safe_char(c: char) -> bool {
    matches!(c,
        '\x21' | '\x23'..='\x2b' | '\x2d'..='\x3a' | '\x3c'..='\x5b' | '\x5d'..='\x7e'
    )
}

/// `path`가 가리키는 **실제 대상**(symlink 해석 후)이 소유자 전용(0600 이하) 권한을 갖는
/// 일반 파일인지 검사한다. age 개인키·웹 토큰 파일 양쪽이 이 하나의 함수를 공유한다.
///
/// [`std::fs::metadata`]는 `stat`(symlink를 따라감)이지 `lstat`(symlink 자체)이 아니다 —
/// 그래서 "symlink는 0600, 가리키는 대상은 0644"인 우회를 별도 `read_link` 루프 없이 이
/// 한 호출로 잡는다. 대상이 없거나(끊어진 symlink 포함) 디렉터리면 거부한다.
fn ensure_secret_file_permissions(path: &Path, label: &str, env_hint: &str) -> Result<()> {
    let meta = std::fs::metadata(path).map_err(|e| {
        XBackupError::Config(format!(
            "{label} 파일을 열 수 없습니다({env_hint}={}): {e} — 경로와 권한을 확인하세요.",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(XBackupError::Config(format!(
            "{label} 경로가 일반 파일이 아닙니다({env_hint}={}) — 디렉터리나 특수 파일이 아닌 \
             파일 하나를 가리켜야 합니다.",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = meta.permissions().mode() & 0o777;
        // 0o077 = 그룹·기타 비트 전부. 0600보다 느슨하면(그룹/기타에 아무 권한이라도 있으면) 거부.
        if mode & 0o077 != 0 {
            return Err(XBackupError::Config(format!(
                "{label} 파일 권한이 너무 느슨합니다(현재 {mode:03o}, 0600이어야 함): {} — \
                 `chmod 0600 {}`로 그룹/기타 권한을 제거하세요(symlink라면 가리키는 실제 파일에 \
                 적용하세요).",
                path.display(),
                path.display()
            )));
        }
    }
    Ok(())
}

/// `XB_AGE_IDENTITY_FILE`이 설정되어 있으면 그 파일의 실권한을 검사한다(symlink 해석 후).
///
/// 이 웹 계층은 age 개인키를 직접 열지 않는다([`crate::web`] 모듈 헤더의 최상위 불변식) —
/// 여기서 하는 일은 "경로가 가리키는 실제 파일이 소유자 전용 권한인가"뿐이다. 실제
/// 복호화는 나중에 자식 프로세스가 같은 env를 읽어 수행한다. 권한이 느슨하면(그룹/기타가
/// 읽을 수 있으면) 로컬의 다른 사용자에게 개인키가 노출된 것이므로, 웹 콘솔을 띄우기 전에
/// 걸러낸다(fail-closed) — 첫 백업/복구를 누른 순간이 아니라 부팅 때 발견해야 한다.
pub fn check_age_identity_permissions() -> Result<()> {
    let env_name = crate::pipeline::stage::ENV_AGE_IDENTITY_FILE;
    let Ok(raw) = std::env::var(env_name) else {
        // 미설정 — 이 배포가 age를 쓰지 않거나(대칭키 경로) 복호화를 다른 호스트가
        // 전담한다는 뜻이다. 웹 콘솔 기동을 막을 이유가 없다.
        return Ok(());
    };
    ensure_secret_file_permissions(Path::new(&raw), "age 개인키", env_name)
}

/// `app_routes()`에 `route_layer`로 걸리는 인증 미들웨어.
///
/// `layer`가 아니라 `route_layer`로 붙어야 한다는 원칙은 [`crate::web::server`] 모듈
/// 헤더가 이미 설명한다 — 여기서는 그 계약을 지키는 쪽(호출부)만 구현한다.
pub async fn require_auth(
    State(auth): State<Arc<AuthState>>,
    request: Request,
    next: Next,
) -> Response {
    let candidate = request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|s| extract_cookie(s, SESSION_COOKIE_NAME));

    // **쿠키에 실린 값은 세션 ID이지 마스터 토큰이 아니다**(모듈 헤더 "쿠키 값은 토큰이
    // 아니다"). 그래서 여기서는 `verify`(토큰)가 아니라 `session_is_valid`를 부른다 —
    // 마스터 토큰을 쿠키에 넣어 보내도 통과하지 않는다. 그 경로를 열어 두면 "쿠키 값이
    // 곧 마스터 자격"이라는 성질이 뒷문으로 되살아난다.
    //
    // 쿠키가 아예 없는 요청은 "실패한 시도"가 아니라 "로그인 안 한 방문"이다.
    let authorized = match candidate {
        Some(session_id) => auth.session_is_valid(&session_id),
        None => false,
    };

    if authorized {
        next.run(request).await
    } else {
        unauthorized_response()
    }
}

// ---------------------------------------------------------------------------
// 출처(origin) 검사 — 포트를 구분하지 않는 쿠키 스코프를 메운다
// ---------------------------------------------------------------------------

/// 상태 변경 요청의 출처를 판정하지 못한 이유.
///
/// 변종을 나눈 이유는 화면이 "무엇을 고쳐야 하는지" 다르게 안내해야 하기 때문이다 —
/// 헤더가 없는 것은 프록시/클라이언트 설정 문제이고, 출처가 다른 것은 실제 위조 시도이거나
/// 잘못된 프록시 호스트 설정이다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OriginProblem {
    /// `Origin`도 `Referer`도 없다 — 브라우저 폼 제출이라면 둘 중 하나는 온다.
    Absent,
    /// `Host` 헤더가 없어 비교 기준을 만들 수 없다(HTTP/1.0 등).
    NoHost,
    /// 출처가 이 콘솔이 아니다. `source`는 **헤더에서 뽑은 authority만** 담는다
    /// (경로·쿼리는 담지 않는다 — 남의 URL 전체를 우리 화면에 그리지 않기 위함이다).
    Foreign { source: String },
}

impl OriginProblem {
    /// 거부 화면·로그에 쓰는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            Self::Absent => lang
                .sel(
                    "This state-changing request carried no Origin or Referer header, so the console cannot tell whether it came from its own pages.",
                    "이 변경 요청에 Origin·Referer 헤더가 없어 콘솔이 자기 화면에서 온 요청인지 판단할 수 없습니다.",
                )
                .to_string(),
            Self::NoHost => lang
                .sel(
                    "This request carried no Host header, so there is nothing to compare the request origin against.",
                    "이 요청에 Host 헤더가 없어 출처를 비교할 기준이 없습니다.",
                )
                .to_string(),
            Self::Foreign { source } => format!(
                "{} ({source})",
                lang.sel(
                    "This state-changing request came from another origin, so it was refused. Note that a service on a different port of the same host counts as another origin here even though the browser shares the cookie with it.",
                    "이 변경 요청은 다른 출처에서 왔으므로 거부했습니다. 브라우저가 쿠키를 공유하더라도, 같은 호스트의 다른 포트는 여기서 다른 출처로 봅니다.",
                )
            ),
        }
    }
}

/// 상태 변경 요청이 이 콘솔 자신의 화면에서 온 것인지 검사한다.
///
/// ## 왜 이 검사가 필요한가 — `SameSite=Strict`가 포트를 못 본다
/// 모듈 헤더 "쿠키 스코프는 포트를 구분하지 않는다"가 근거다. 같은 호스트의 다른 포트에서
/// 도는 서비스는 브라우저 입장에서 **same-site**이므로 `SameSite=Strict`가 그쪽에서 오는
/// 상태 변경 요청을 막지 않는다. 그런데 `Origin`은 **스킴 + 호스트 + 포트**로 정의되므로
/// (RFC 6454) 정확히 그 구멍만 메운다 — 막아야 하는 것이 "포트가 다른 same-site"이므로,
/// 포트를 보는 신호를 쓰는 것이 이 문제의 정확한 해법이다.
///
/// ## 왜 CSRF 토큰이 아닌가
/// 리더가 제시한 선택지는 CSRF 토큰이었다. 같은 목적을 달성하지만 이 코드베이스에서는
/// 출처 검사가 낫다고 판단했다:
///
/// 1. **폼을 고치지 않아도 된다.** 토큰 방식은 상태 변경 폼 **전부**(`view::config`의 저장·
///    삭제, `view::backup`의 실행·취소)에 hidden 필드를 심어야 하고, 하나만 빠뜨리면 그
///    화면이 403으로 죽는다. 출처 검사는 브라우저가 이미 보내는 헤더만 보므로 폼이 하나도
///    바뀌지 않고, **앞으로 추가될 POST도 자동으로 보호된다**(토큰은 새 폼마다 다시
///    기억해야 한다).
/// 2. **새 의존성이 없다.** 토큰을 제대로 하려면 요청마다 예측 불가능한 값이 필요하고,
///    그건 CSPRNG(직접 의존성 추가) 얘기로 돌아간다. 세션 토큰을 그대로 hidden 필드에 넣는
///    편법은 **그 값을 HTML 본문·브라우저 히스토리·프록시 로그로 퍼뜨리는** 정반대 방향의
///    사고다.
/// 3. 두 방어는 배타적이지 않다 — 불투명 세션 ID를 도입하는 후속 태스크에서 토큰을 함께
///    넣고 싶으면 이 검사는 그대로 남겨 두면 된다(다층).
///
/// ## 비교 기준을 `Host` 헤더로 잡는 이유
/// `ctx.bind`와 비교하면 **프록시 뒤에서 항상 틀린다** — 권장 배치에서 브라우저가 보는
/// 출처는 `https://console.example.com`이고 우리 bind는 `127.0.0.1:8787`이다. 그래서
/// 브라우저가 보낸 `Origin`을 같은 요청의 `Host`(프록시가 `proxy_set_header Host $host`로
/// 넘겨주는 브라우저용 호스트)와 비교한다 — OWASP가 권하는 방식이고, 직접 노출·프록시 뒤
/// 양쪽에서 성립한다.
///
/// **`Host`를 신뢰해도 되는 이유**: 공격자가 `Host`와 `Origin`을 둘 다 자기 값으로 맞춰
/// 보낼 수는 있지만, 그건 CSRF가 아니다. CSRF는 **피해자의 브라우저가 피해자의 쿠키를
/// 실어 보내게** 만드는 공격이고, 그 브라우저는 `Host`를 **실제 목적지**로 채운다(공격자가
/// 고를 수 없다). 반면 `Origin`은 공격 페이지가 있는 곳이 되므로 둘이 어긋난다. 헤더를
/// 임의로 만들 수 있는 상대(브라우저가 아닌 클라이언트)는 애초에 이 검사의 대상이 아니다 —
/// 그 상대가 쿠키까지 갖고 있다면 CSRF를 할 이유가 없다.
///
/// ## 판정 순서와 fail-closed
/// `Origin` → (없으면) `Referer` → (둘 다 없으면) **거부**. 브라우저의 폼 제출은 최소 하나를
/// 보낸다(`Origin`은 모든 POST에, `Referer`는 참조자 정책이 막지 않는 한). 둘 다 없는
/// 요청을 통과시키면 "헤더를 안 보내면 우회된다"가 되어 검사 자체가 무의미해진다.
///
/// 스킴은 비교하지 않는다 — 막아야 하는 것은 포트가 다른 same-site이고, 같은 authority에
/// 스킴만 다른 출처가 동시에 존재하는 배치는 현실적으로 없다. 대신 authority는
/// **대소문자를 무시**해 비교한다(호스트명은 대소문자를 구분하지 않는다).
pub fn verify_same_origin(headers: &HeaderMap) -> std::result::Result<(), OriginProblem> {
    let expected = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|host| !host.is_empty())
        .ok_or(OriginProblem::NoHost)?;

    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|v| v.to_str().ok())
        .ok_or(OriginProblem::Absent)?;

    // `Origin: null`(불투명 출처 — sandbox iframe·file:// 등)은 `://`가 없으므로 여기서
    // authority를 뽑지 못하고 그대로 거부된다. 의도된 동작이다.
    let Some(authority) = authority_of_url(source) else {
        return Err(OriginProblem::Foreign {
            source: source.trim().to_string(),
        });
    };
    if authority.eq_ignore_ascii_case(expected) {
        return Ok(());
    }
    Err(OriginProblem::Foreign {
        source: authority.to_string(),
    })
}

/// `scheme://authority/path?query`에서 authority(`host[:port]`)만 뽑는다.
///
/// 직접 파싱하는 이유: URL 파서 crate를 새로 들이지 않고, 우리가 필요한 것은 첫 `://` 뒤
/// 첫 구분자 전까지 한 조각뿐이다. userinfo(`user@host`)는 `Origin`/`Referer`에 올 수 없는
/// 형태이므로 다루지 않는다(오면 authority가 달라져 거부되는 안전한 방향이다).
fn authority_of_url(raw: &str) -> Option<&str> {
    let rest = raw.trim().split_once("://")?.1;
    let authority = rest
        .split(['/', '?', '#'])
        .next()
        .filter(|a| !a.is_empty())?;
    Some(authority)
}

/// 이 메서드가 서버 상태를 바꿀 수 있는가 — 출처 검사를 적용할 대상 판정.
///
/// 안전한 메서드(GET/HEAD/OPTIONS)만 면제한다. 이 콘솔은 GET/POST만 쓰지만, 목록을
/// "면제할 것"으로 두는 편이 안전하다 — 나중에 PUT/DELETE가 추가되면 자동으로 검사 대상이
/// 된다(반대로 "검사할 것" 목록이면 조용히 빠진다. `job::runner`의 화이트리스트와 같은 방향의
/// 판단이다).
fn is_state_changing(method: &axum::http::Method) -> bool {
    use axum::http::Method;
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

/// 상태 변경 요청에 [`verify_same_origin`]을 강제하는 미들웨어.
///
/// **아직 라우터에 배선되어 있지 않다** — `src/web/server.rs`가 이 태스크의 소관 밖이라
/// 리더가 `app_routes`에 한 줄(`.route_layer(middleware::from_fn(auth::require_same_origin))`)을
/// 얹어야 전체 POST(로그인·백업 실행·취소 포함)가 덮인다. 그 배선 없이도 **config 저장·삭제
/// 두 경로는 이미 보호된다** — 그 핸들러들이 같은 검사를 직접 부른다
/// (`crate::web::routes::config`). 가장 되돌릴 수 없는 두 작업을 먼저 덮어 둔 것이다.
pub async fn require_same_origin(request: Request, next: Next) -> Response {
    if is_state_changing(request.method()) {
        if let Err(problem) = verify_same_origin(request.headers()) {
            tracing::warn!(
                method = %request.method(),
                path = request.uri().path(),
                reason = ?problem,
                "출처를 확인할 수 없어 상태 변경 요청을 거부했습니다"
            );
            return forbidden_response(&problem);
        }
    }
    next.run(request).await
}

/// 출처 검사 실패 응답. 미들웨어 경로는 화면 골격 없이 평문으로 답한다 —
/// 이 응답을 받는 쪽은 대개 사람이 보는 브라우저 화면이 아니라 위조된 요청이다
/// ([`unauthorized_response`]와 같은 판단).
fn forbidden_response(problem: &OriginProblem) -> Response {
    (
        StatusCode::FORBIDDEN,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("forbidden — {}\n", problem.explain(Lang::En)),
    )
        .into_response()
}

/// 인증 실패 응답 — 토큰·쿠키 값은 절대 담지 않는다("인증 실패" 수준으로만, 태스크 지침).
fn unauthorized_response() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/plain; charset=utf-8")],
        format!("unauthorized — sign in at {LOGIN_PATH}\n"),
    )
        .into_response()
}

/// `Cookie` 헤더 문자열에서 이름이 일치하는 값을 하나 뽑는다. `str::split`/`split_once`는
/// 항상 UTF-8 문자 경계에서만 잘리므로(패턴이 ASCII 문자일 때) 이 파싱은 어떤 입력에도
/// 패닉하지 않는다.
fn extract_cookie(header: &str, name: &str) -> Option<String> {
    header.split(';').find_map(|kv| {
        let (k, v) = kv.trim().split_once('=')?;
        (k == name).then(|| v.to_string())
    })
}

/// 세션 쿠키의 `Secure` 정책. 근거는 모듈 헤더 "쿠키 속성 근거" 참조.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SecurePolicy {
    /// 요청 스킴(`X-Forwarded-Proto`)으로 판정한다 — 기본값.
    Auto,
    /// 항상 붙인다(프록시가 스킴 헤더를 전달하지 않는 TLS 배치).
    Always,
    /// 절대 붙이지 않는다(TLS 없이 원격 노출한, 권장하지 않는 배치).
    Never,
}

impl SecurePolicy {
    /// [`ENV_WEB_COOKIE_SECURE`]를 읽는다. 모르는 값은 **거부하지 않고** `Auto`로 떨어뜨리되
    /// 경고를 남긴다 — 여기서 기동을 막으면 오타 하나가 콘솔을 못 띄우게 만드는데, 이
    /// 설정은 fail-closed로 다룰 대상(인증 유무)이 아니라 판정 힌트다.
    fn from_env() -> Self {
        match std::env::var(ENV_WEB_COOKIE_SECURE) {
            Ok(raw) => match raw.trim().to_ascii_lowercase().as_str() {
                "" | "auto" => Self::Auto,
                "always" | "1" | "true" => Self::Always,
                "never" | "0" | "false" => Self::Never,
                other => {
                    tracing::warn!(
                        value = %other,
                        "{ENV_WEB_COOKIE_SECURE}의 값을 알 수 없어 auto로 처리합니다 — \
                         auto|always|never 중 하나를 쓰세요"
                    );
                    Self::Auto
                }
            },
            Err(_) => Self::Auto,
        }
    }
}

/// 이 요청이 브라우저-프록시 구간에서 https였는지 — `X-Forwarded-Proto`로 판정한다.
///
/// 값이 `https,http`처럼 여러 홉으로 이어질 수 있으므로 **첫 조각**(브라우저에 가장 가까운
/// 홉)만 본다. 이 헤더를 신뢰하는 근거는 모듈 헤더 참조.
fn request_is_https(headers: &HeaderMap) -> bool {
    headers
        .get(HEADER_FORWARDED_PROTO)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(',').next())
        .is_some_and(|proto| proto.trim().eq_ignore_ascii_case("https"))
}

/// 이 응답의 세션 쿠키에 `Secure`를 붙일지 정한다(정책 → 요청 스킴 순).
fn cookie_secure(headers: &HeaderMap) -> bool {
    match SecurePolicy::from_env() {
        SecurePolicy::Always => true,
        SecurePolicy::Never => false,
        SecurePolicy::Auto => request_is_https(headers),
    }
}

/// 이 요청이 "TLS 없이 원격에 노출된 콘솔"에 도달한 것인지 — 로그인 화면의 경고 판정.
///
/// 루프백 바인딩은 대상이 아니다(로컬 평문은 의도된 기본 배치다). 그리고 `X-Forwarded-Proto`
/// 가 https면 앞단에 TLS가 있다는 뜻이므로 역시 대상이 아니다.
fn plaintext_remote_exposure(ctx: &ServeConfig, headers: &HeaderMap) -> bool {
    !ctx.bind.ip().is_loopback() && !request_is_https(headers)
}

/// `GET /login` — 로그인 폼. [`crate::web::server::public_routes`]에 실려 인증 없이 열린다.
pub async fn login_form(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    uri: Uri,
) -> Markup {
    let failed = uri
        .query()
        .is_some_and(|q| q.split('&').any(|pair| pair == "error=1"));
    render_login_page(ctx.lang, failed, &login_warnings(&ctx, &headers))
}

/// 로그인 화면에 띄울 경고 목록(없으면 빈 슬라이스).
///
/// **이 경고들이 존재하는 이유는 진단과 고지다.** `Secure`를 평문 요청에 내려보내면 브라우저가
/// 쿠키를 버려 `303 → / → 401`이 조용히 무한 반복되는데(리뷰 실측), 그 상태에서 화면에는
/// 아무 원인도 나오지 않는다. 그 침묵이 최악의 진단 경험이므로 위험한 조합마다 이름을 붙여
/// 로그인 화면에 그대로 적는다.
///
/// 하나가 아니라 목록인 이유: 전송 구간(스킴)과 쿠키 스코프(포트)는 **서로 독립적인 문제**라
/// 동시에 성립할 수 있다. 평문 원격 노출이면 둘 다 뜬다.
///
/// [`TransportWarning::SharedCookieScope`]를 **루프백에서는 띄우지 않는다.** 그 성질은
/// 루프백에서도 성립하지만(`127.0.0.1:3000`의 개발 서버도 쿠키를 받는다), 권장 기본 배치인
/// 로컬 단독 사용에 상시 경고를 띄우면 경고 인플레이션이 되어 정작 위험한 배치의 경고까지
/// 무시된다. 루프백 쪽 성질은 모듈 헤더에 적어 두고 화면에서는 침묵한다.
fn login_warnings(ctx: &ServeConfig, headers: &HeaderMap) -> Vec<TransportWarning> {
    let https = request_is_https(headers);
    let mut warnings = Vec::new();
    // 평문 요청인데 Secure를 강제하도록 설정됐다 — 로그인이 완료될 수 없는 조합이므로 먼저.
    if SecurePolicy::from_env() == SecurePolicy::Always && !https {
        warnings.push(TransportWarning::SecureForcedOverPlaintext);
    }
    if plaintext_remote_exposure(ctx, headers) {
        warnings.push(TransportWarning::PlaintextRemote);
    }
    if !ctx.bind.ip().is_loopback() {
        warnings.push(TransportWarning::SharedCookieScope);
    }
    warnings
}

/// 로그인 화면에 표시하는 경고의 종류.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TransportWarning {
    /// TLS 없이 원격에 노출됨 — 로그인 시 토큰이, 이후에는 세션 ID가 평문으로 왕복한다.
    PlaintextRemote,
    /// `Secure`를 강제했는데 이 요청은 평문 — 브라우저가 쿠키를 버려 로그인이 반복된다.
    SecureForcedOverPlaintext,
    /// 원격 노출 — 같은 호스트의 다른 포트 서비스도 이 세션 쿠키를 받는다(모듈 헤더 참조).
    ///
    /// 완화가 아니라 **고지**다. 쿠키 값이 불투명 세션 ID로 바뀐 뒤로 유출의 무게는
    /// 크게 줄었지만(마스터 토큰이 아니고, 만료되고, 회수 가능하다) 성질 자체는 남는다 —
    /// 그 세션이 살아 있는 동안은 콘솔을 쓸 수 있다. 운영자는 알아야 판단할 수 있다:
    /// 콘솔에 전용 호스트명을 주거나 이 호스트에 다른 서비스를 두지 않는 선택은 이 사실을
    /// 아는 사람만 할 수 있다.
    SharedCookieScope,
}

impl TransportWarning {
    /// 경고 제목(짧게).
    fn title(self, lang: Lang) -> &'static str {
        match self {
            Self::PlaintextRemote => lang.sel("Unencrypted connection", "암호화되지 않은 연결"),
            Self::SecureForcedOverPlaintext => {
                lang.sel("Login cannot complete", "로그인이 완료될 수 없습니다")
            }
            Self::SharedCookieScope => lang.sel(
                "This session cookie is shared with other ports",
                "이 세션 쿠키는 다른 포트와 공유됩니다",
            ),
        }
    }

    /// 무엇이 왜 위험한지 + 무엇을 고치면 되는지.
    fn detail(self, lang: Lang) -> String {
        match self {
            Self::PlaintextRemote => lang.sel(
                "This console is bound to a non-loopback address and this request arrived over plain http, so the access token travels unencrypted. Put a TLS reverse proxy in front and have it send `X-Forwarded-Proto: https`.",
                "이 콘솔은 루프백이 아닌 주소에 바인딩됐고 이 요청은 평문 http로 도착했습니다 — 접근 토큰이 암호화되지 않은 채 왕복합니다. 앞단에 TLS 리버스 프록시를 두고 `X-Forwarded-Proto: https`를 전달하게 하세요.",
            ),
            Self::SecureForcedOverPlaintext => lang.sel(
                "XB_WEB_COOKIE_SECURE=always is set but this request arrived over plain http. The browser will discard a `Secure` cookie on an unencrypted connection, so signing in will keep bouncing back to this page. Serve the console over https (or set the variable to auto/never).",
                "XB_WEB_COOKIE_SECURE=always가 설정되어 있지만 이 요청은 평문 http로 도착했습니다. 브라우저는 암호화되지 않은 연결에서 `Secure` 쿠키를 버리므로, 로그인해도 이 화면으로 계속 되돌아옵니다. https로 서빙하거나 이 변수를 auto/never로 바꾸세요.",
            ),
            Self::SharedCookieScope => lang.sel(
                "Browser cookies are not isolated by port: any other HTTP service on this same hostname (for example :3000) also receives this session cookie. The value is a session id, not the master token — it expires and can be revoked — but whoever holds it can use this console until then. Give the console its own hostname, or keep other services off this host. State-changing requests are refused unless they come from this exact origin, port included.",
                "브라우저 쿠키는 포트로 격리되지 않습니다: 같은 호스트명의 다른 HTTP 서비스(예: :3000)도 이 세션 쿠키를 함께 받습니다. 값은 마스터 토큰이 아니라 만료·회수되는 세션 ID이지만, 가진 쪽은 그때까지 이 콘솔을 쓸 수 있습니다. 콘솔에 전용 호스트명을 주거나 이 호스트에 다른 서비스를 두지 마세요. 변경 요청은 포트까지 일치하는 이 출처에서 온 것만 허용됩니다.",
            ),
        }
        .to_string()
    }
}

/// `POST /login` — 토큰을 검증하고, 성공하면 세션 쿠키를 심어 `/`로 303 리다이렉트한다.
///
/// 실패해도 폼 필드를 그대로 되돌려주지 않는다 — `?error=1`만 붙여 `GET /login`으로 303
/// 리다이렉트한다(폼 재제출 확인창을 브라우저가 띄우지 않도록 POST-Redirect-GET 패턴).
///
/// `HeaderMap`을 받는 이유는 `Secure` 판정이 **요청 스킴**에 달려 있기 때문이다(모듈 헤더
/// "쿠키 속성 근거"). axum 핸들러는 마지막 하나(`body: String`)만 본문을 소비하는 추출자면
/// 되므로, 라우터 등록(`src/web/server.rs`)은 손대지 않고 추출자만 늘릴 수 있다.
pub async fn login_submit(
    State(ctx): State<Arc<ServeConfig>>,
    Extension(auth): Extension<Arc<AuthState>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let candidate = form_field(&body, "token").unwrap_or_default();
    if auth.verify(&candidate).await {
        let secure = cookie_secure(&headers);
        // 두 위험한 조합은 화면(로그인 폼)에만 적지 않고 로그에도 남긴다 — 브라우저 앞에
        // 앉은 사람과 로그를 뒤지는 사람이 서로 다를 수 있고, 무한 반복은 대개 로그로
        // 먼저 발견된다(303이 끝없이 찍힌다).
        if secure && !request_is_https(&headers) {
            tracing::warn!(
                "세션 쿠키에 Secure를 붙였지만 이 요청은 평문 http입니다 — 브라우저가 쿠키를 \
                 버리므로 로그인이 303 → / → 401로 반복됩니다. https로 서빙하거나 \
                 {ENV_WEB_COOKIE_SECURE}를 auto/never로 두세요."
            );
        } else if !secure && !ctx.bind.ip().is_loopback() {
            tracing::warn!(
                "루프백이 아닌 주소에 바인딩된 콘솔에 평문 http로 로그인했습니다 — 세션 \
                 쿠키에 Secure를 붙이지 않았고(붙이면 브라우저가 버려 로그인이 반복됩니다), \
                 따라서 접근 토큰이 평문으로 왕복합니다. 앞단 TLS 프록시가 \
                 X-Forwarded-Proto: https를 전달하게 하세요."
            );
        }
        // 스킴과 무관한 별개의 고지 — 원격 노출이면 항상 남긴다(모듈 헤더 "쿠키 스코프는
        // 포트를 구분하지 않는다"). 세션이 만들어지는 순간이 이 사실을 알려야 하는 시점이다.
        if !ctx.bind.ip().is_loopback() {
            tracing::warn!(
                "이 세션 쿠키는 같은 호스트명의 **다른 포트** 서비스에도 함께 전송됩니다 \
                 (브라우저 쿠키는 포트로 격리되지 않는다 — RFC 6265 §8.5, 실측 확인). \
                 쿠키 값은 마스터 토큰이 아니라 회수 가능한 세션 ID이므로 유출돼도 토큰 \
                 자체는 남지 않지만, 그 세션으로 콘솔을 쓸 수는 있습니다 — 콘솔에 전용 \
                 호스트명을 주거나 이 호스트에 다른 HTTP 서비스를 두지 마세요."
            );
        }
        // 쿠키에 실리는 것은 **여기서 새로 만든 세션 ID**다. 제출된 토큰(`candidate`)은
        // 이 함수 밖으로 나가지 않는다 — 그것이 이 설계의 요점이다.
        let session_id = match auth.issue_session() {
            Ok(id) => id,
            Err(e) => {
                tracing::error!("세션 발급 실패: {e}");
                return (
                    StatusCode::SEE_OTHER,
                    [(header::LOCATION, format!("{LOGIN_PATH}?error=1"))],
                )
                    .into_response();
            }
        };
        let cookie = build_session_cookie(&session_id, secure);
        (
            StatusCode::SEE_OTHER,
            [
                (header::LOCATION, "/".to_string()),
                (header::SET_COOKIE, cookie),
            ],
        )
            .into_response()
    } else {
        (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, format!("{LOGIN_PATH}?error=1"))],
        )
            .into_response()
    }
}

/// 세션 쿠키의 `Set-Cookie` 값을 만든다. 속성 근거는 모듈 헤더 참조.
///
/// 인자는 **세션 ID**다(마스터 토큰이 아니다) — 이름이 `token`이던 시절의 설계가 정확히
/// 이 파일이 고친 취약점이었다.
fn build_session_cookie(session_id: &str, secure: bool) -> String {
    let mut cookie =
        format!("{SESSION_COOKIE_NAME}={session_id}; Path=/; HttpOnly; SameSite=Strict");
    if secure {
        cookie.push_str("; Secure");
    }
    cookie
}

/// 로그인 화면 마크업. `view::layout` 파일을 건드리지 않고 기존 `shell`/`.card` 골격과
/// [`components::notice`]만 재사용한다.
///
/// **마크업은 모양을 모른다** — 이전 판본은 실패 문구에 `style="color: var(--c-fail)"`를
/// 달았는데, 그것이 전체 웹 마크업에서 유일한 인라인 `style=`이었다. 색 리터럴이 아니라
/// 변수를 참조하니 app.css 계층 계약은 깨지지 않았지만, "마크업은 레벨(의미)만 말하고
/// 모양은 CSS가 정한다"는 규약을 깨는 자리였다. `notice(Level::Fail, …)`가 정확히 그 일을
/// 한다 — `data-level="fail"`을 실어 보내고 색은 app.css가 정한다.
fn render_login_page(lang: Lang, failed: bool, warnings: &[TransportWarning]) -> Markup {
    super::view::layout::shell(
        lang,
        "Sign in",
        html! {
            div class="card" {
                @for warning in warnings {
                    (components::notice(
                        Level::Warn,
                        warning.title(lang),
                        html! { p { (warning.detail(lang)) } },
                    ))
                }
                @if failed {
                    (components::notice(
                        Level::Fail,
                        lang.sel("Invalid token.", "토큰이 올바르지 않습니다."),
                        html! {},
                    ))
                }
                form method="post" action=(LOGIN_PATH) {
                    label for="token" { (lang.sel("Access token", "접근 토큰")) }
                    br;
                    input type="password" id="token" name="token" autocomplete="off" required;
                    button type="submit" { (lang.sel("Sign in", "로그인")) }
                }
            }
        },
    )
}

/// `application/x-www-form-urlencoded` 본문에서 필드 하나만 뽑아 디코드한다.
///
/// axum `form` feature(→ `serde_urlencoded` 의존성)를 새로 켜는 대신, 로그인 폼이 필요로
/// 하는 필드 하나만 직접 파싱한다 — `Cargo.toml`은 이 태스크 소관 밖이고, 새 의존성을
/// 더할 만큼 파싱이 복잡하지 않다.
fn form_field(body: &str, field: &str) -> Option<String> {
    let field = field.as_bytes();
    body.as_bytes().split(|&b| b == b'&').find_map(|pair| {
        let eq = pair.iter().position(|&b| b == b'=')?;
        let (k, v) = pair.split_at(eq);
        (k == field).then(|| percent_decode(&v[1..]))
    })
}

/// `+`(공백) / `%XX`(퍼센트 인코딩)를 디코드한다.
///
/// 바이트(`&[u8]`) 단위로만 다룬다 — `&str` 슬라이싱(`s[i..j]`)은 UTF-8 문자 경계를
/// 벗어나면 패닉한다. 이 함수는 어떤 입력(잘못 인코딩된 멀티바이트 문자 포함)에도
/// 패닉하지 않아야 하므로(공인되지 않은 사용자가 두드리는 `POST /login` 본문이다),
/// 인덱스를 바이트로만 계산하고 마지막에 [`String::from_utf8_lossy`]로 한 번만 접는다.
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
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// ASCII 16진 숫자 한 글자를 값으로. 호출부가 `is_ascii_hexdigit()`로 이미 걸러내므로
/// 그 외 입력에는 도달하지 않는다(도달해도 0을 돌려줄 뿐 패닉하지 않는다).
fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// 강도 하한을 통과하는 표본 토큰 — `openssl rand -hex 12`가 내놓는 모양(24자).
    const STRONG_TOKEN: &str = "9f2b7c41ae08d53b6c1f4a92";

    fn clear_auth_env() {
        std::env::remove_var(ENV_WEB_TOKEN);
        std::env::remove_var(ENV_WEB_TOKEN_FILE);
        std::env::remove_var(ENV_WEB_COOKIE_SECURE);
    }

    /// `X-Forwarded-Proto` 하나만 담은 헤더맵.
    fn headers_with_proto(proto: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_FORWARDED_PROTO, proto.parse().unwrap());
        headers
    }

    /// 루프백이 아닌 주소에 바인딩된 테스트 컨텍스트(원격 노출 배치).
    fn remote_ctx() -> ServeConfig {
        let mut ctx = ServeConfig::for_test();
        ctx.bind = "203.0.113.7:8787".parse().unwrap();
        ctx
    }

    // ---- 세션(불투명 ID) ----

    /// **이 파일이 고친 취약점을 못박는 테스트.**
    ///
    /// 예전에는 세션 쿠키 값이 마스터 토큰 그 자체였다. 브라우저 쿠키는 포트로 격리되지
    /// 않으므로(RFC 6265 §8.5 — 실제 Chrome으로 확인했다: :A가 심은 쿠키를 같은 호스트 :B가
    /// 그대로 받는다), 같은 호스트명의 다른 포트에서 도는 아무 HTTP 서비스나 그 쿠키를
    /// 함께 받았고 그 값이 곧 전 백업 복호화 자격이었다.
    ///
    /// 지금은 쿠키에 회수 가능한 세션 ID만 실린다. 그래서 **토큰을 쿠키에 넣어도 통과하면
    /// 안 된다** — 통과한다면 옛 성질이 뒷문으로 살아 있다는 뜻이다.
    #[test]
    fn the_master_token_is_not_accepted_as_a_session_cookie() {
        const TOKEN: &str = "master-token-that-must-not-work-as-a-cookie";
        let (auth, session) = AuthState::for_test_with_session(TOKEN);

        assert!(auth.session_is_valid(&session), "정상 세션이 거부됐다");
        assert!(
            !auth.session_is_valid(TOKEN),
            "마스터 토큰이 세션 쿠키로 통과했다 — 고친 취약점이 되살아났다"
        );
    }

    /// 발급된 세션 ID는 토큰과 아무 관계가 없고, 매번 다르다.
    #[test]
    fn issued_sessions_are_random_and_unrelated_to_the_token() {
        const TOKEN: &str = "some-master-token-value-1234567890";
        let auth = AuthState::for_test(TOKEN);

        let first = auth.issue_session().unwrap();
        let second = auth.issue_session().unwrap();

        assert_ne!(first, second, "두 세션이 같은 값이다 — 난수가 아니다");
        assert_eq!(first.len(), SESSION_ID_BYTES * 2, "hex 길이가 다르다");
        assert!(
            !first.contains(TOKEN) && !TOKEN.contains(&first),
            "세션 ID가 토큰에서 유도된 것처럼 보인다"
        );
        assert!(
            first.chars().all(|c| c.is_ascii_hexdigit()),
            "쿠키에 안전하지 않은 문자가 섞였다: {first}"
        );
    }

    /// 형식이 어긋난 쿠키 값은 조용히 거부된다(패닉하지 않는다).
    #[test]
    fn malformed_session_cookies_are_rejected_without_panicking() {
        let (auth, session) = AuthState::for_test_with_session("token-for-malformed-test-01");

        for bad in [
            "",
            "not-hex-at-all",
            "zz",
            &"f".repeat(SESSION_ID_BYTES * 2 - 1), // 한 글자 짧다
            &"f".repeat(SESSION_ID_BYTES * 2 + 1), // 한 글자 길다
            &"g".repeat(SESSION_ID_BYTES * 2),     // 길이는 맞지만 hex가 아니다
        ] {
            assert!(!auth.session_is_valid(bad), "{bad:?}가 통과했다");
        }
        // 망가진 입력들을 흘려보낸 뒤에도 정상 세션은 그대로 산다.
        assert!(auth.session_is_valid(&session));
    }

    /// 세션은 [`SESSION_TTL`]이 지나면 만료된다 — 탈취돼도 영원히 살지 않는다.
    #[test]
    fn sessions_expire_after_the_ttl() {
        let auth = AuthState::for_test("token-for-expiry-test-abcdef");
        let start = Instant::now();
        let session = auth.issue_session_at(start).unwrap();

        // 상한 직전에는 살아 있다.
        let almost = start + SESSION_TTL - Duration::from_secs(1);
        assert!(
            auth.session_is_valid_at(&session, almost),
            "아직 만료 전인데 거부됐다"
        );

        // 상한을 넘기면 죽는다.
        let after = start + SESSION_TTL + Duration::from_secs(1);
        assert!(
            !auth.session_is_valid_at(&session, after),
            "만료된 세션이 통과했다"
        );
    }

    /// 만료된 세션은 저장소에서도 걷힌다 — 로그인만 반복해도 메모리가 자라지 않는다.
    #[test]
    fn expired_sessions_are_swept_out_of_the_store() {
        let auth = AuthState::for_test("token-for-sweep-test-abcdef12");
        let start = Instant::now();
        auth.issue_session_at(start).unwrap();
        assert_eq!(auth.session_count(), 1);

        // TTL을 넘긴 시점에 새로 발급하면, 그 과정에서 옛 세션이 걷힌다.
        auth.issue_session_at(start + SESSION_TTL + Duration::from_secs(1))
            .unwrap();
        assert_eq!(auth.session_count(), 1, "만료된 세션이 남아 있다");
    }

    /// 세션 수는 상한을 넘지 않고, 넘치면 **가장 오래된 것부터** 밀려난다.
    #[test]
    fn session_count_is_capped_and_evicts_the_oldest_first() {
        let auth = AuthState::for_test("token-for-cap-test-0123456789");
        let oldest = auth.issue_session().unwrap();

        for _ in 1..MAX_SESSIONS {
            auth.issue_session().unwrap();
        }
        assert_eq!(auth.session_count(), MAX_SESSIONS);
        assert!(auth.session_is_valid(&oldest), "아직 밀려날 차례가 아니다");

        let newest = auth.issue_session().unwrap();
        assert_eq!(auth.session_count(), MAX_SESSIONS, "상한을 넘었다");
        assert!(
            !auth.session_is_valid(&oldest),
            "상한을 넘겼는데 가장 오래된 세션이 살아남았다"
        );
        assert!(auth.session_is_valid(&newest), "방금 만든 세션이 없다");
    }

    /// 한 세션을 밀어내도 다른 세션들은 그대로 유효하다(전부 날아가지 않는다).
    #[test]
    fn evicting_one_session_does_not_disturb_the_others() {
        let auth = AuthState::for_test("token-for-evict-test-abcdef00");
        let sessions: Vec<String> = (0..MAX_SESSIONS)
            .map(|_| auth.issue_session().unwrap())
            .collect();

        auth.issue_session().unwrap(); // 가장 오래된 하나를 밀어낸다

        assert!(!auth.session_is_valid(&sessions[0]));
        for surviving in &sessions[1..] {
            assert!(auth.session_is_valid(surviving), "무관한 세션이 죽었다");
        }
    }

    /// hex 왕복이 값을 바꾸지 않는다.
    #[test]
    fn hex_round_trips() {
        let bytes = [0u8, 1, 15, 16, 127, 128, 254, 255];
        let mut id = [0u8; SESSION_ID_BYTES];
        id[..bytes.len()].copy_from_slice(&bytes);

        let text = hex_encode(&id);
        assert_eq!(text.len(), SESSION_ID_BYTES * 2);
        assert_eq!(hex_decode(&text), Some(id));

        // 대문자 hex도 받는다 — 우리는 소문자로 내지만, 중간 계층이 대소문자를 바꿔도
        // 세션이 조용히 죽지 않아야 한다.
        assert_eq!(hex_decode(&text.to_uppercase()), Some(id));
    }

    // ---- constant_time_eq ----

    #[test]
    fn constant_time_eq_matches_equal_bytes() {
        assert!(constant_time_eq(b"same-token", b"same-token"));
    }

    #[test]
    fn constant_time_eq_rejects_first_byte_mismatch() {
        assert!(!constant_time_eq(b"Xame-token", b"same-token"));
    }

    #[test]
    fn constant_time_eq_rejects_last_byte_mismatch() {
        // 첫 바이트가 아니라 마지막 바이트만 다른 경우도 동일하게 false여야 한다 — 조기
        // 반환 없이 전체를 본다는 것의 최소 회귀 테스트.
        assert!(!constant_time_eq(b"same-tokeX", b"same-token"));
    }

    #[test]
    fn constant_time_eq_rejects_different_lengths() {
        assert!(!constant_time_eq(b"short", b"much-longer-token"));
    }

    #[test]
    fn constant_time_eq_empty_slices_are_equal() {
        assert!(constant_time_eq(b"", b""));
    }

    // ---- failure_delay (순수 함수) ----

    #[test]
    fn failure_delay_is_zero_below_threshold() {
        assert_eq!(failure_delay(1), Duration::ZERO);
        assert_eq!(failure_delay(FAILURE_THRESHOLD - 1), Duration::ZERO);
    }

    #[test]
    fn failure_delay_grows_after_threshold() {
        let at_threshold = failure_delay(FAILURE_THRESHOLD);
        let one_more = failure_delay(FAILURE_THRESHOLD + 1);
        assert!(at_threshold > Duration::ZERO, "문턱에서는 지연이 있어야 함");
        assert!(one_more > at_threshold, "실패가 늘수록 지연도 늘어야 함");
    }

    #[test]
    fn failure_delay_is_capped_at_max() {
        assert_eq!(failure_delay(10_000), MAX_DELAY);
    }

    // ---- AuthState::verify (async, 실제 지연 관찰) ----

    #[tokio::test]
    async fn verify_accepts_matching_token() {
        let auth = AuthState::for_test("correct-horse");
        assert!(auth.verify("correct-horse").await);
    }

    #[tokio::test]
    async fn verify_rejects_wrong_token() {
        let auth = AuthState::for_test("correct-horse");
        assert!(!auth.verify("wrong").await);
    }

    #[tokio::test]
    async fn verify_trims_candidate_whitespace() {
        // 붙여넣기로 흔히 섞이는 개행/공백은 후보 쪽만 관대하게 자른다(저장된 토큰 자체는
        // load_from_env가 이미 trim했으므로 대칭적이다).
        let auth = AuthState::for_test("correct-horse");
        assert!(auth.verify(" correct-horse\n").await);
    }

    /// **성공이 실패 카운터를 리셋하지 않는다** — H5-3의 회귀 테스트.
    ///
    /// 이전 판본은 성공 시 `store(0)`을 했고, `require_auth`가 매 요청 `verify`를 부르므로
    /// 운영자의 정상 사용(특히 SSE 재연결)이 공격자의 누적 지연을 계속 지웠다. 리셋을
    /// 되살리면 이 테스트가 깨진다.
    #[tokio::test]
    async fn success_does_not_clear_the_failure_counter() {
        let auth = AuthState::for_test("correct-horse");
        for _ in 0..FAILURE_THRESHOLD {
            assert!(!auth.verify("wrong").await);
        }
        let before = auth.failure_count();
        assert_eq!(before, FAILURE_THRESHOLD);

        assert!(
            auth.verify("correct-horse").await,
            "맞는 토큰은 통과해야 함"
        );
        assert_eq!(
            auth.failure_count(),
            before,
            "성공이 카운터를 되돌리면 정상 트래픽이 방어를 무력화한다"
        );
    }

    /// 카운터를 내려주는 유일한 힘은 시간이다.
    ///
    /// [`AuthState::note_failure`]가 `now`를 인자로 받는 이유가 이 테스트다 — 감쇠를 실제로
    /// 몇 분 기다리지 않고 확인할 수 있다(`tokio::time::advance`는 `test-util` feature가
    /// 필요한데 이 크레이트의 tokio는 `full`만 켜져 있고 `Cargo.toml`은 이 태스크 범위 밖이다).
    #[test]
    fn note_failure_applies_decay_between_attempts() {
        let auth = AuthState::for_test("correct-horse");
        let t0 = Instant::now();
        for step in 0..5 {
            // 같은 순간에 연달아 실패하면 감쇠 없이 그대로 쌓인다.
            auth.note_failure(t0);
            assert_eq!(auth.failure_count(), step + 1);
        }

        // 감쇠 주기 3배가 지난 뒤의 실패 → 3칸이 빠지고 그 자리에 1이 얹힌다.
        auth.note_failure(t0 + FAILURE_DECAY_PERIOD * 3);
        assert_eq!(
            auth.failure_count(),
            5 - 3 + 1,
            "감쇠가 반영되지 않으면 사람의 오타가 영구히 남는다"
        );

        // 충분히 오래 쉬면 처음 상태로 돌아간다(첫 실패와 같은 지연 = 없음).
        assert_eq!(
            auth.note_failure(t0 + FAILURE_DECAY_PERIOD * 1_000),
            Duration::ZERO
        );
    }

    /// 감쇠 정책 자체(순수 함수) — 시계 없이 경계값을 고정한다.
    #[test]
    fn decay_failures_removes_one_step_per_period() {
        assert_eq!(decay_failures(5, Duration::ZERO), 5);
        // 주기 직전까지는 한 칸도 빠지지 않는다.
        assert_eq!(
            decay_failures(5, FAILURE_DECAY_PERIOD - Duration::from_secs(1)),
            5
        );
        assert_eq!(decay_failures(5, FAILURE_DECAY_PERIOD), 4);
        assert_eq!(decay_failures(5, FAILURE_DECAY_PERIOD * 4), 1);
        // 아무리 오래 지나도 음수로 내려가지 않는다(포화).
        assert_eq!(decay_failures(5, FAILURE_DECAY_PERIOD * 1_000), 0);
        assert_eq!(decay_failures(0, FAILURE_DECAY_PERIOD), 0);
    }

    /// **병렬 공격이 지연을 공짜로 만들지 못한다** — H5-2의 회귀 테스트.
    ///
    /// 실측된 옛 동작: 동시 100회 실패 = 2.03초(각 요청이 자기 sleep만 await하므로 전체가
    /// 가장 긴 지연 하나로 끝난다). 지금은 실패가 지연을 소화하는 동안 세마포어 허가를
    /// 들고 있으므로, 전체 시간의 하한이 **총 지연 ÷ 허가 수**로 내려간다.
    ///
    /// 12회 동시 실패의 지연 합은 0+0+200+400+…+2000 = 11,000ms이고 허가는 4개이므로
    /// makespan ≥ 2,750ms다(스케줄링은 이 값을 늘릴 수만 있다). 병렬로 새면 가장 긴 지연
    /// 하나(2,000ms)에 끝나므로 이 하한이 두 동작을 가른다.
    ///
    /// **이 테스트는 실제로 2.7초 이상 걸린다.** 가상 시계(`tokio::time::pause`)를 쓰려면
    /// tokio의 `test-util` feature가 필요한데 이 크레이트는 `full`만 켜져 있고 `Cargo.toml`은
    /// 이 태스크 범위 밖이다. 병렬 공격이 공짜였다는 것이 실측으로 확인된 구멍이라, 느린
    /// 대신 실제 시간을 재는 이 테스트를 남기는 편이 낫다고 판단했다.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_failures_cannot_share_one_delay() {
        const ATTEMPTS: usize = 12;
        let auth = AuthState::for_test("correct-horse");

        let started = Instant::now();
        let mut handles = Vec::with_capacity(ATTEMPTS);
        for _ in 0..ATTEMPTS {
            let auth = Arc::clone(&auth);
            handles.push(tokio::spawn(async move { auth.verify("wrong").await }));
        }
        for handle in handles {
            assert!(!handle.await.unwrap(), "틀린 토큰이 통과했다");
        }
        let elapsed = started.elapsed();

        assert!(
            elapsed >= Duration::from_millis(2_400),
            "동시 실패가 지연 하나를 공유했다(병렬 공격이 공짜다): {elapsed:?}"
        );
    }

    /// 공격이 허가를 다 점유한 순간에도 **맞는 토큰은 지연 없이 통과한다** — 동시 상한이
    /// 정상 사용을 인질로 잡지 않는다는 것이 이 방어의 전제다(모듈 헤더 3번).
    ///
    /// 그리고 같은 상황에서 **추가 실패는 줄을 선다**(거부되지 않는다) — 그것이 처리량
    /// 상한이 실제로 걸린다는 증거다. 두 성질을 한 셋업에서 함께 확인한다.
    #[tokio::test]
    async fn attempt_slots_queue_failures_without_blocking_success() {
        let auth = AuthState::for_test("correct-horse");
        // 먼저 카운터를 문턱 위로 올려, 이후 실패들이 실제로 허가를 잡고 자게 만든다.
        for _ in 0..FAILURE_THRESHOLD {
            assert!(!auth.verify("wrong").await);
        }
        for _ in 0..MAX_CONCURRENT_ATTEMPTS {
            let auth = Arc::clone(&auth);
            tokio::spawn(async move { auth.verify("wrong").await });
        }
        // 허가가 전부 잡힐 때까지 양보한다.
        while auth.attempt_slots.available_permits() > 0 {
            tokio::task::yield_now().await;
        }

        // 1) 맞는 토큰은 세마포어를 지나지 않으므로 즉시 통과한다.
        let started = Instant::now();
        assert!(auth.verify("correct-horse").await, "맞는 토큰이 거부됐다");
        assert!(
            started.elapsed() < BASE_DELAY,
            "성공 경로가 동시 상한에 걸렸다 — 공격자가 콘솔을 잠글 수 있다: {:?}",
            started.elapsed()
        );

        // 2) 허가가 없는 실패는 짧은 창 안에 끝나지 못한다(줄을 선다).
        let queued = tokio::time::timeout(BASE_DELAY / 2, auth.verify("wrong")).await;
        assert!(
            queued.is_err(),
            "허가가 없는데도 실패 처리가 즉시 진행됐다 — 처리량 상한이 걸리지 않는다"
        );
    }

    // ---- 토큰 강도 하한 (H5-1) ----

    #[test]
    fn entropy_estimate_rejects_degenerate_tokens_and_passes_random_ones() {
        // 한 글자 반복은 0비트.
        assert_eq!(estimated_entropy_bits(&"a".repeat(MIN_TOKEN_LEN)), 0.0);
        // 두 글자 교대는 길이당 1비트 = 24비트.
        assert!(estimated_entropy_bits(&"ab".repeat(12)) < MIN_TOKEN_ENTROPY_BITS);
        // 랜덤 hex 24자(≈91비트)·base64 24자는 여유롭게 통과한다.
        assert!(estimated_entropy_bits(STRONG_TOKEN) >= MIN_TOKEN_ENTROPY_BITS);
        assert!(estimated_entropy_bits("Kp3sQ9wZ1mNvB7xL4hRt2Cd8") >= MIN_TOKEN_ENTROPY_BITS);
        assert_eq!(estimated_entropy_bits(""), 0.0);
    }

    /// 빈도 기반 하한이 못 보는 **배열 구조**를 짝 검사가 잡는다.
    ///
    /// `passwordpasswordpassword`는 문자 7종이라 빈도만 보면 약 66비트로 하한을 통과한다 —
    /// 이 테스트가 두 검사를 함께 두어야 하는 이유를 고정한다.
    #[test]
    fn repeated_pattern_is_rejected_even_when_frequency_looks_fine() {
        let repeated = "passwordpasswordpassword";
        assert!(
            estimated_entropy_bits(repeated) >= MIN_TOKEN_ENTROPY_BITS,
            "이 표본은 빈도 하한을 통과해야 이 테스트가 의미가 있다"
        );
        assert!(has_repeated_period(repeated));
        assert!(
            ensure_token_strength(repeated).is_err(),
            "반복 패턴이 통과했다"
        );

        // 다른 반복 형태들.
        assert!(has_repeated_period(&"a".repeat(MIN_TOKEN_LEN)));
        assert!(has_repeated_period(&"ab".repeat(12)));
        assert!(has_repeated_period("halfhalf"), "2회 반복도 반복이다");
        // 난수 문자열은 걸리지 않는다(오탐 방지).
        assert!(!has_repeated_period(STRONG_TOKEN));
        assert!(!has_repeated_period("Kp3sQ9wZ1mNvB7xL4hRt2Cd8"));
        assert!(
            !has_repeated_period("a"),
            "한 글자는 반복 판정 대상이 아니다"
        );
    }

    #[test]
    fn load_from_env_rejects_token_shorter_than_minimum() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        // 리뷰 실측이 통과시킨 값 — 한 글자 토큰으로 기동해 200을 받았다.
        std::env::set_var(ENV_WEB_TOKEN, "a");
        let err = AuthState::load_from_env().expect_err("한 글자 토큰이 통과하면 안 됨");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(
            err.to_string().contains(&MIN_TOKEN_LEN.to_string()),
            "하한 값을 알려주지 않는다: {err}"
        );
        // 하한 바로 아래도 거부된다(경계).
        std::env::set_var(ENV_WEB_TOKEN, "b".repeat(MIN_TOKEN_LEN - 1));
        assert!(AuthState::load_from_env().is_err(), "하한-1이 통과했다");
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_long_but_monotonous_token() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        // 길이만 채운 토큰 — 길이 하한만 있으면 통과한다.
        std::env::set_var(ENV_WEB_TOKEN, "a".repeat(MIN_TOKEN_LEN * 2));
        let err = AuthState::load_from_env().expect_err("단조로운 토큰이 통과하면 안 됨");
        assert!(
            err.to_string().contains("엔트로피"),
            "거부 이유가 엔트로피임을 말하지 않는다: {err}"
        );
        clear_auth_env();
    }

    /// 강도 검사가 토큰 값을 메시지에 담지 않는다(기동 실패 메시지는 로그에 남는다).
    #[test]
    fn token_strength_errors_never_echo_the_token() {
        let weak = "sekret";
        let err = ensure_token_strength(weak).expect_err("짧은 토큰은 거부되어야 함");
        assert!(
            !err.to_string().contains(weak),
            "거부 메시지에 토큰 값이 들어갔다: {err}"
        );
    }

    #[tokio::test]
    async fn verify_inserts_delay_after_threshold_failures() {
        let auth = AuthState::for_test("correct-horse");
        // 문턱 미만은 눈에 띄는 지연이 없어야 한다.
        let start = std::time::Instant::now();
        for _ in 0..(FAILURE_THRESHOLD - 1) {
            assert!(!auth.verify("wrong").await);
        }
        assert!(
            start.elapsed() < BASE_DELAY,
            "문턱 이전 실패에는 지연이 없어야 함"
        );

        // 문턱을 넘는 순간부터는 measurable한 지연이 응답 전에 끼어야 한다.
        let start = std::time::Instant::now();
        assert!(!auth.verify("wrong").await);
        assert!(
            start.elapsed() >= BASE_DELAY / 2,
            "문턱을 넘은 실패는 지연이 있어야 함: {:?}",
            start.elapsed()
        );
    }

    // ---- AuthState::load_from_env ----

    #[test]
    fn load_from_env_fails_closed_without_any_source() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        let err = AuthState::load_from_env().expect_err("설정 없이 통과하면 안 됨");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(err.to_string().contains(ENV_WEB_TOKEN));
        assert!(err.to_string().contains(ENV_WEB_TOKEN_FILE));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_both_sources_set() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN, "a-token");
        std::env::set_var(ENV_WEB_TOKEN_FILE, "/tmp/does-not-matter");
        let err = AuthState::load_from_env().expect_err("둘 다 설정되면 모호함으로 거부");
        assert!(err.to_string().contains("동시에"));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_empty_token() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN, "");
        let err = AuthState::load_from_env().expect_err("빈 토큰은 거부되어야 함");
        assert!(err.to_string().contains("비어"));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_whitespace_only_token() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN, "   \n\t  ");
        let err = AuthState::load_from_env().expect_err("공백만 있는 토큰은 거부되어야 함");
        assert!(err.to_string().contains("비어"));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_cookie_unsafe_char() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN, "has;semicolon");
        let err = AuthState::load_from_env().expect_err("세미콜론 포함 토큰은 거부되어야 함");
        assert!(err.to_string().contains("쿠키"));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_accepts_direct_token() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN, STRONG_TOKEN);
        let auth = AuthState::load_from_env().expect("유효한 토큰은 통과해야 함");
        assert!(constant_time_eq(&auth.token, STRONG_TOKEN.as_bytes()));
        clear_auth_env();
    }

    #[test]
    fn load_from_env_reads_token_from_0600_file() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token");
        std::fs::write(&path, format!("{STRONG_TOKEN}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::env::set_var(ENV_WEB_TOKEN_FILE, path.to_str().unwrap());

        let auth = AuthState::load_from_env().expect("0600 파일은 통과해야 함");
        assert!(
            constant_time_eq(&auth.token, STRONG_TOKEN.as_bytes()),
            "개행이 trim되지 않았다"
        );
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_token_file_with_loose_permissions() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("token");
        std::fs::write(&path, "file-token").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::env::set_var(ENV_WEB_TOKEN_FILE, path.to_str().unwrap());

        let err = AuthState::load_from_env().expect_err("0644 토큰 파일은 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_missing_token_file() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_TOKEN_FILE, "/nonexistent/path/token");
        let err = AuthState::load_from_env().expect_err("없는 파일은 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        clear_auth_env();
    }

    #[test]
    fn load_from_env_rejects_directory_as_token_file() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        let tmp = tempfile::tempdir().unwrap();
        std::env::set_var(ENV_WEB_TOKEN_FILE, tmp.path().to_str().unwrap());
        let err = AuthState::load_from_env().expect_err("디렉터리는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        clear_auth_env();
    }

    // ---- ensure_secret_file_permissions (경로만 받는 순수 함수 — env를 만지지 않는다) ----
    //
    // age 개인키 검사에 요구된 4개 케이스(0644 거부·symlink 우회 거부·부재 거부·디렉터리
    // 거부)를 전부 여기서 env 없이 검증한다. env를 거치는 `check_age_identity_permissions`는
    // `XB_AGE_IDENTITY_FILE`을 다른 파일(`pipeline::stage` 등)의 테스트와 공유하므로, 그
    // 경로를 건드리는 테스트는 아래에서 최소 1개(통합 확인용)로 줄인다.

    #[test]
    fn ensure_secret_file_permissions_accepts_0600() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("key");
        std::fs::write(&path, b"identity").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(ensure_secret_file_permissions(&path, "테스트", "TEST_ENV").is_ok());
    }

    #[test]
    fn ensure_secret_file_permissions_rejects_0644() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("key");
        std::fs::write(&path, b"identity").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = ensure_secret_file_permissions(&path, "테스트", "TEST_ENV")
            .expect_err("0644는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    #[test]
    fn ensure_secret_file_permissions_rejects_symlink_to_loose_target() {
        // symlink 자체 권한이 아니라 "가리키는 대상"의 권한을 봐야 한다 — 이 테스트가
        // 핵심 함정을 고정한다(모듈 헤더 참조).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-key");
        std::fs::write(&target, b"identity").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o644)).unwrap();
        let link = tmp.path().join("key-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let err = ensure_secret_file_permissions(&link, "테스트", "TEST_ENV")
            .expect_err("symlink 대상이 0644면 symlink 경유도 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    #[test]
    fn ensure_secret_file_permissions_accepts_symlink_to_0600_target() {
        // 대칭 케이스 — 대상이 0600이면 symlink를 통해서도 통과해야 한다(과도한 거부 방지).
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("real-key");
        std::fs::write(&target, b"identity").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = tmp.path().join("key-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        assert!(ensure_secret_file_permissions(&link, "테스트", "TEST_ENV").is_ok());
    }

    #[test]
    fn ensure_secret_file_permissions_rejects_missing_file() {
        let err = ensure_secret_file_permissions(
            Path::new("/nonexistent/path/key"),
            "테스트",
            "TEST_ENV",
        )
        .expect_err("부재 파일은 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    #[test]
    fn ensure_secret_file_permissions_rejects_broken_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let link = tmp.path().join("broken-link");
        std::os::unix::fs::symlink(tmp.path().join("does-not-exist"), &link).unwrap();
        let err = ensure_secret_file_permissions(&link, "테스트", "TEST_ENV")
            .expect_err("끊어진 symlink는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
    }

    #[test]
    fn ensure_secret_file_permissions_rejects_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let err = ensure_secret_file_permissions(tmp.path(), "테스트", "TEST_ENV")
            .expect_err("디렉터리는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(err.to_string().contains("디렉터리"));
    }

    // ---- check_age_identity_permissions (env 경유 — 최소 케이스만) ----

    #[test]
    fn check_age_identity_permissions_ok_when_unset() {
        let _guard = ENV_GUARD.lock().unwrap();
        std::env::remove_var(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);
        assert!(check_age_identity_permissions().is_ok());
    }

    #[test]
    fn check_age_identity_permissions_rejects_loose_target_via_env() {
        // 이 하나가 "env 배선이 ensure_secret_file_permissions로 실제로 이어지는가"를
        // 확인한다(0644·symlink·부재·디렉터리 4개 케이스 자체는 env 없이 위에서 이미 다
        // 검증했다).
        let _guard = ENV_GUARD.lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("identity");
        std::fs::write(&path, b"age-key").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::env::set_var(
            crate::pipeline::stage::ENV_AGE_IDENTITY_FILE,
            path.to_str().unwrap(),
        );

        let err = check_age_identity_permissions().expect_err("0644 age 개인키는 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);

        std::env::remove_var(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);
    }

    // ---- 쿠키/폼 파싱 ----

    #[test]
    fn extract_cookie_finds_value_among_multiple() {
        let header = "foo=bar; xb_session=the-token; baz=qux";
        assert_eq!(
            extract_cookie(header, SESSION_COOKIE_NAME),
            Some("the-token".to_string())
        );
    }

    #[test]
    fn extract_cookie_returns_none_when_absent() {
        assert_eq!(extract_cookie("foo=bar", SESSION_COOKIE_NAME), None);
        assert_eq!(extract_cookie("", SESSION_COOKIE_NAME), None);
    }

    #[test]
    fn form_field_decodes_plus_and_percent() {
        assert_eq!(
            form_field("token=a%2Bb+c%3D", "token"),
            Some("a+b c=".to_string())
        );
    }

    #[test]
    fn form_field_returns_none_for_missing_field() {
        assert_eq!(form_field("other=value", "token"), None);
    }

    #[test]
    fn percent_decode_does_not_panic_on_truncated_multibyte_sequence() {
        // '%' 뒤에 완전한 hex 두 자리가 없고, 곧바로 3바이트 UTF-8 문자(€)가 이어지는
        // 입력 — 잘못 구현하면 &str 슬라이싱이 문자 경계를 가로질러 패닉한다.
        let input = "€".as_bytes();
        let mut malformed = vec![b'%'];
        malformed.extend_from_slice(input);
        let decoded = percent_decode(&malformed);
        assert_eq!(decoded, "%€", "패닉 없이 원문에 가깝게 통과해야 함");
    }

    #[test]
    fn percent_decode_handles_trailing_incomplete_percent() {
        assert_eq!(percent_decode(b"abc%"), "abc%");
        assert_eq!(percent_decode(b"abc%2"), "abc%2");
    }

    // ---- Secure 쿠키 판정 (H6) ----

    /// **판정은 바인딩이 아니라 요청 스킴을 본다** — H6(a)의 회귀 테스트.
    ///
    /// 권장 배치(`프록시(443) → 127.0.0.1:8787`)는 루프백 바인딩 + https 요청이다. 옛
    /// 판정(`!bind.ip().is_loopback()`)은 여기서 정확히 `Secure`를 껐다.
    #[test]
    fn secure_follows_request_scheme_not_bind_address() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();

        // 루프백 바인딩 + 프록시가 알려준 https → Secure를 붙인다(옛 판정은 껐다).
        assert!(cookie_secure(&headers_with_proto("https")));
        // 평문 요청 → 붙이지 않는다(붙이면 브라우저가 쿠키를 버려 로그인이 반복된다).
        assert!(!cookie_secure(&headers_with_proto("http")));
        assert!(!cookie_secure(&HeaderMap::new()));
        // 대소문자·공백은 관대하게, 다중 홉은 브라우저에 가장 가까운 첫 조각만 본다.
        assert!(cookie_secure(&headers_with_proto("HTTPS")));
        assert!(cookie_secure(&headers_with_proto(" https , http ")));
        assert!(!cookie_secure(&headers_with_proto("http, https")));

        clear_auth_env();
    }

    /// 명시 설정은 요청 스킴 판정을 완전히 대체한다(프록시가 헤더를 안 보내는 배치용).
    #[test]
    fn cookie_secure_env_overrides_scheme_detection() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();

        std::env::set_var(ENV_WEB_COOKIE_SECURE, "always");
        assert!(cookie_secure(&HeaderMap::new()), "always가 무시됐다");

        std::env::set_var(ENV_WEB_COOKIE_SECURE, "never");
        assert!(
            !cookie_secure(&headers_with_proto("https")),
            "never가 무시됐다"
        );

        // 모르는 값은 기동을 막지 않고 auto로 떨어진다(판정 힌트이지 인증 유무가 아니다).
        std::env::set_var(ENV_WEB_COOKIE_SECURE, "yes-please");
        assert!(cookie_secure(&headers_with_proto("https")));
        assert!(!cookie_secure(&HeaderMap::new()));

        clear_auth_env();
    }

    /// **평문으로 원격 노출된 배치는 화면에 원인이 적힌다** — H6(b)의 회귀 테스트.
    ///
    /// 옛 동작은 원격 노출에서 `Secure` 쿠키를 내려보내 `303 → / → 401`이 조용히 반복됐고,
    /// 원인 메시지가 어디에도 없었다. 지금은 (1) 쿠키에 `Secure`를 붙이지 않아 로그인이
    /// 되고, (2) 왜 위험한지가 로그인 화면에 적힌다.
    #[test]
    fn plaintext_remote_exposure_is_explained_on_the_login_page() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        let ctx = remote_ctx();

        let warnings = login_warnings(&ctx, &HeaderMap::new());
        assert!(warnings.contains(&TransportWarning::PlaintextRemote));

        let page = render_login_page(Lang::Ko, false, &warnings).into_string();
        assert!(page.contains("암호화되지 않은"), "경고 제목 누락: {page}");
        assert!(
            page.contains("X-Forwarded-Proto"),
            "해결 방법이 적혀 있지 않다: {page}"
        );
        assert!(
            page.contains(r#"data-level="warn""#),
            "경고 레벨이 마크업에 없다"
        );

        // 앞단에 TLS가 있으면(https로 도착) 전송 구간 경고는 사라진다 — 쿠키 스코프 고지는
        // 스킴과 무관하므로 남는다(M9).
        let with_tls = login_warnings(&ctx, &headers_with_proto("https"));
        assert!(!with_tls.contains(&TransportWarning::PlaintextRemote));
        assert_eq!(with_tls, vec![TransportWarning::SharedCookieScope]);
        // 루프백 평문은 의도된 기본 배치다 — 아무 경고도 띄우지 않는다(경고 인플레이션 방지).
        assert!(login_warnings(&ServeConfig::for_test(), &HeaderMap::new()).is_empty());

        clear_auth_env();
    }

    /// `Secure` 강제 + 평문 요청 = 로그인이 완료될 수 없는 조합이므로, 그 사실을 화면이
    /// 직접 말한다(조용히 반복되는 로그인 실패가 최악의 진단 경험이다).
    #[test]
    fn forced_secure_over_plaintext_names_the_cause() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();
        std::env::set_var(ENV_WEB_COOKIE_SECURE, "always");

        let warnings = login_warnings(&ServeConfig::for_test(), &HeaderMap::new());
        assert_eq!(warnings, vec![TransportWarning::SecureForcedOverPlaintext]);
        let page = render_login_page(Lang::En, false, &warnings).into_string();
        assert!(page.contains(ENV_WEB_COOKIE_SECURE), "원인 변수 이름 누락");
        assert!(
            page.contains("discard"),
            "브라우저가 쿠키를 버린다는 사실 누락"
        );

        // https로 도착하면 이 조합이 아니다.
        assert!(login_warnings(&ServeConfig::for_test(), &headers_with_proto("https")).is_empty());

        clear_auth_env();
    }

    #[test]
    fn session_cookie_carries_expected_attributes() {
        let plain = build_session_cookie("t", false);
        assert!(plain.contains("HttpOnly") && plain.contains("SameSite=Strict"));
        assert!(!plain.contains("Secure"), "평문 요청에 Secure가 붙었다");
        assert!(build_session_cookie("t", true).contains("; Secure"));
    }

    // ---- 출처 검사 (M9) ----

    /// `Host` + 선택적 `Origin`/`Referer`를 담은 헤더맵.
    fn origin_headers(host: &str, origin: Option<&str>, referer: Option<&str>) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if !host.is_empty() {
            headers.insert(header::HOST, host.parse().unwrap());
        }
        if let Some(origin) = origin {
            headers.insert(header::ORIGIN, origin.parse().unwrap());
        }
        if let Some(referer) = referer {
            headers.insert(header::REFERER, referer.parse().unwrap());
        }
        headers
    }

    /// **포트가 다른 same-site 출처를 거부한다** — M9의 핵심.
    ///
    /// 브라우저 쿠키는 포트로 격리되지 않으므로 `:3000`에서 온 POST에도 세션 쿠키가 실린다.
    /// `SameSite=Strict`는 그것을 same-site로 보아 막지 못한다 — `Origin`은 포트를 포함하므로
    /// 정확히 그 구멍만 메운다.
    #[test]
    fn same_origin_check_distinguishes_ports() {
        // 같은 authority = 통과.
        assert_eq!(
            verify_same_origin(&origin_headers(
                "console.example:8787",
                Some("https://console.example:8787"),
                None
            )),
            Ok(())
        );
        // 포트만 다르다 = 거부(M9이 지적한 그 경로).
        assert_eq!(
            verify_same_origin(&origin_headers(
                "console.example:8787",
                Some("https://console.example:3000"),
                None
            )),
            Err(OriginProblem::Foreign {
                source: "console.example:3000".to_string()
            })
        );
        // 호스트가 다르면 당연히 거부.
        assert!(verify_same_origin(&origin_headers(
            "console.example:8787",
            Some("https://evil.example:8787"),
            None
        ))
        .is_err());
        // 호스트명은 대소문자를 구분하지 않는다.
        assert_eq!(
            verify_same_origin(&origin_headers(
                "Console.Example:8787",
                Some("https://console.example:8787"),
                None
            )),
            Ok(())
        );
        // 스킴은 비교하지 않는다(막아야 하는 것은 포트다 — 함수 doc 참조).
        assert_eq!(
            verify_same_origin(&origin_headers(
                "console.example:8787",
                Some("http://console.example:8787"),
                None
            )),
            Ok(())
        );
    }

    /// `Origin`이 없으면 `Referer`로 판정하고, 둘 다 없으면 **거부**한다(fail-closed).
    #[test]
    fn same_origin_check_falls_back_to_referer_then_refuses() {
        // Referer는 경로가 붙어 오지만 authority만 본다.
        assert_eq!(
            verify_same_origin(&origin_headers(
                "console.example:8787",
                None,
                Some("https://console.example:8787/config/edit/prod?x=1")
            )),
            Ok(())
        );
        assert!(verify_same_origin(&origin_headers(
            "console.example:8787",
            None,
            Some("https://console.example:3000/app")
        ))
        .is_err());
        // 둘 다 없으면 통과시키지 않는다 — 통과시키면 "헤더를 빼면 우회된다"가 된다.
        assert_eq!(
            verify_same_origin(&origin_headers("console.example:8787", None, None)),
            Err(OriginProblem::Absent)
        );
        // 비교 기준(Host)이 없으면 판정 자체가 성립하지 않는다.
        assert_eq!(
            verify_same_origin(&origin_headers(
                "",
                Some("https://console.example:8787"),
                None
            )),
            Err(OriginProblem::NoHost)
        );
        // 불투명 출처(`Origin: null` — sandbox iframe 등)도 거부된다.
        assert!(
            verify_same_origin(&origin_headers("console.example:8787", Some("null"), None))
                .is_err()
        );
    }

    /// 세 거부 이유가 서로 다른 문장을 낸다(운영자가 의심할 곳이 달라진다).
    #[test]
    fn origin_problems_explain_themselves_distinctly() {
        let problems = [
            OriginProblem::Absent,
            OriginProblem::NoHost,
            OriginProblem::Foreign {
                source: "console.example:3000".to_string(),
            },
        ];
        for (i, a) in problems.iter().enumerate() {
            for lang in [Lang::En, Lang::Ko] {
                assert!(!a.explain(lang).is_empty());
            }
            for b in &problems[i + 1..] {
                assert_ne!(a.explain(Lang::Ko), b.explain(Lang::Ko));
            }
        }
        // 거부 사유에는 출처 authority만 들어간다(남의 URL 전체를 화면에 그리지 않는다).
        let foreign = OriginProblem::Foreign {
            source: "console.example:3000".to_string(),
        };
        assert!(foreign.explain(Lang::Ko).contains("console.example:3000"));
    }

    /// 안전한 메서드는 검사 대상이 아니고, 그 밖의 메서드는 전부 대상이다.
    #[test]
    fn only_state_changing_methods_are_checked() {
        use axum::http::Method;
        for safe in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(
                !is_state_changing(&safe),
                "{safe} 이 검사 대상이 되면 화면이 열리지 않는다"
            );
        }
        for unsafe_method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(
                is_state_changing(&unsafe_method),
                "{unsafe_method} 이 빠졌다"
            );
        }
    }

    /// 미들웨어가 POST만 막고 GET은 통과시킨다(실제 axum 스택으로 확인).
    #[tokio::test]
    async fn require_same_origin_middleware_guards_posts_only() {
        use axum::body::Body;
        use axum::routing::get;
        use axum::{middleware, Router};
        use tower::ServiceExt;

        let app = Router::new()
            .route("/", get(|| async { "ok" }).post(|| async { "changed" }))
            .layer(middleware::from_fn(require_same_origin));

        let call = |method: &str, headers: HeaderMap| {
            let mut builder = axum::http::Request::builder().method(method).uri("/");
            for (name, value) in headers.iter() {
                builder = builder.header(name, value);
            }
            let request = builder.body(Body::empty()).unwrap();
            let app = app.clone();
            async move { app.oneshot(request).await.unwrap().status() }
        };

        // GET은 출처 헤더가 없어도 열린다(모든 화면이 그렇다).
        assert_eq!(call("GET", HeaderMap::new()).await, StatusCode::OK);
        // POST는 출처가 없으면 403.
        assert_eq!(call("POST", HeaderMap::new()).await, StatusCode::FORBIDDEN);
        // 다른 포트에서 온 POST도 403.
        assert_eq!(
            call(
                "POST",
                origin_headers(
                    "console.example:8787",
                    Some("https://console.example:3000"),
                    None
                )
            )
            .await,
            StatusCode::FORBIDDEN
        );
        // 자기 화면에서 온 POST는 통과.
        assert_eq!(
            call(
                "POST",
                origin_headers(
                    "console.example:8787",
                    Some("https://console.example:8787"),
                    None
                )
            )
            .await,
            StatusCode::OK
        );
    }

    /// 원격 노출이면 **스킴과 무관하게** 쿠키 스코프 고지가 뜬다(M9 고지 요구).
    #[test]
    fn remote_exposure_discloses_the_shared_cookie_scope() {
        let _guard = ENV_GUARD.lock().unwrap();
        clear_auth_env();

        // TLS가 제대로 붙은 원격 배치에도 이 고지는 남는다 — 전송 구간과 무관한 문제다.
        let warnings = login_warnings(&remote_ctx(), &headers_with_proto("https"));
        assert_eq!(warnings, vec![TransportWarning::SharedCookieScope]);
        let page = render_login_page(Lang::Ko, false, &warnings).into_string();
        assert!(page.contains("다른 포트"), "포트 공유 사실이 없다: {page}");
        assert!(page.contains(":3000"), "구체적인 예시가 없다: {page}");

        // 루프백에서는 띄우지 않는다(경고 인플레이션 방지 — 함수 doc의 판단).
        assert!(
            !login_warnings(&ServeConfig::for_test(), &headers_with_proto("https"))
                .contains(&TransportWarning::SharedCookieScope)
        );

        clear_auth_env();
    }

    // ---- 마크업 규약 (H8) ----

    /// **마크업에 인라인 `style=`이 없다** — 전체 웹 마크업의 유일한 예외였던 자리다.
    /// 실패 문구는 색을 직접 정하지 않고 `notice(Level::Fail, …)`의 레벨만 실어 보낸다.
    #[test]
    fn login_page_has_no_inline_styles() {
        for failed in [true, false] {
            for warnings in [
                Vec::new(),
                vec![
                    TransportWarning::PlaintextRemote,
                    TransportWarning::SharedCookieScope,
                ],
            ] {
                let page = render_login_page(Lang::Ko, failed, &warnings).into_string();
                assert!(!page.contains("style="), "인라인 style이 남아 있다: {page}");
                assert!(!page.contains("--c-"), "색 변수가 마크업에 남아 있다");
            }
        }
        let failed = render_login_page(Lang::Ko, true, &[]).into_string();
        assert!(
            failed.contains(r#"data-level="fail""#),
            "실패 알림이 fail 레벨로 나가지 않는다: {failed}"
        );
        assert!(failed.contains("토큰이 올바르지 않습니다"));
    }
}
