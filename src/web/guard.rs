//! 파괴적 작업 가드 — CLI의 "명시 플래그를 타이핑해야 함"을 웹의 "클릭 한 번"으로부터
//! 지킨다.
//!
//! ## 이 모듈이 왜 필요한가
//! CLI는 파괴적 작업(`restore --allow-overwrite`, `migrate --drop-target`, `prune --force`)을
//! 실행하려면 사람이 터미널에 그 플래그를 **직접 타이핑**해야 한다. 실수로 엔터를 두 번
//! 치는 것과 실수로 그 문자열을 타이핑하는 것 사이에는 진짜 마찰이 있다. 웹은 그 마찰이
//! 버튼 클릭 한 번으로 사라진다 — 이 모듈은 그 격차를 메우려고 존재한다. `restore`(t25)·
//! `prune`(t23)·`migrate`(t24) 세 태스크가 각자 구현을 반복하지 않도록, "확인 화면을
//! 보여주고 → 제출을 검증하고 → 감사 게이트를 통과한 [`AuditReceipt`]를 손에 쥐는" 흐름
//! 전체를 여기 한 곳에 묶는다. 호출부(t23~25)는 "무엇을 확인시킬지"만 채워 넣으면 된다
//! ([`DestructiveRequest`]).
//!
//! ## 설계 결정 1 — 2단계 확인 + 프로파일명 재입력
//! [`DestructiveGuard::render_confirm`]이 실행 직전 확인 화면을 만든다. 그 화면은 (1) 무엇이
//! 어느 프로파일에 대해 일어나는지, (2) 되돌릴 수 없는 부분이 무엇인지를 말하고
//! ([`components::verdict_banner`]를 [`Level::Fail`]로 — `view::config`의 삭제 확인 화면과
//! 같은 판단, 삭제만큼 되돌릴 수 없는 작업이라는 뜻이다), (3) 대상 프로파일명을 그대로
//! 입력하게 요구한다. 그 입력은 **서버 측에서 정확히 일치하는지 재검증한다**
//! ([`DestructiveGuard::confirm_and_gate`]) — 클라이언트 쪽 JS 검증(애초에 이 콘솔엔 JS가
//! 없다, [`crate::web::view`] 모듈 헤더 참조)만으로는 우회되므로, 유일한 검증 지점이 서버다.
//! 이 패턴은 `routes::config::delete_submit`(t26/t27)이 먼저 만든 최소 버전을 그대로
//! 승격한 것이다 — 그 파일의 doc이 "이 확인 단계는 t21이 승격시킬 대상"이라고 명시했다.
//!
//! ## 설계 결정 2 — 재인증 대신 일회용 확인 토큰 (선택지 c)
//! 리더가 제시한 선택지는 셋이었다: (a) 토큰 재입력을 요구하되 한계를 명시, (b) 재인증 없이
//! 이름 타이핑 + 토글만, (c) 확인 화면이 발급한 일회용 토큰. **(c)를 골랐다.**
//!
//! (a)를 버린 이유(**당시 사정**): 그때 이 콘솔의 세션 쿠키 값은 **설정된 마스터 토큰 그
//! 자체**였다. 확인 화면에서 "토큰을 다시 입력하세요"라고 물으면, 브라우저는 이미 그 값을
//! 쿠키로 들고 있고 사람은 그 값을 어딘가에서 복사해 붙여넣는다 — 같은 값을 같은 세션 안에서
//! 두 번 확인하는 것은 새로운 지식을 증명하지 않는다. 그래서 (a)는 거짓 안전감을 준다고 보고
//! 버렸다.
//!
//! **그 사정은 이제 바뀌었다.** 세션 쿠키 값은 마스터 토큰이 아니라 불투명 세션 ID다
//! ([`crate::web::auth`] 모듈 헤더 "쿠키 값은 토큰이 아니다"). 즉 "토큰을 다시 입력하라"가
//! 이제는 **쿠키가 모르는 값**을 요구하는 것이 되어, 진짜 두 번째 요소로 성립한다 — 그
//! 자리가 열렸다. 다만 그것이 아래 (c)를 대체하지는 않는다: 재인증은 "이 사람이 토큰을
//! 아는가"를 묻고, (c)는 "이 확인이 이 실행과 짝인가"를 묻는다. 서로 다른 질문이라 겹쳐
//! 쌓을 수 있고, 지금 구현된 것은 (c)뿐이다.
//!
//! (b)를 버린 이유: 프로파일명 타이핑만으로는 "확인 화면을 실제로 봤다"는 사실과 "그
//! 확인에 대한 응답으로 실행됐다"는 사실을 요청 하나로 묶지 못한다 — 화면을 한 번 열어 둔
//! 채 그 URL/폼을 북마크하거나 브라우저 뒤로가기 후 재제출하면, 이미 지나간 확인이 다시
//! 발동한다(재생). 이름 타이핑 자체는 "실수 방지"이지 "이 확인 인스턴스가 그 실행과 짝인가"
//! 를 보장하지 않는다.
//!
//! (c)를 고른 이유: [`DestructiveGuard::render_confirm`]이 확인 화면을 그릴 **때** 토큰을
//! 하나 발급해 서버 메모리에 보관하고([`PendingConfirmation`]), 그 토큰은 (1) 딱 그 확인
//! 인스턴스에만 유효하고, (2) 제출 한 번으로 **소비**되어(성공하든 거부되든) 재사용이
//! 불가능하고, (3) 유효 시간이 [`CONFIRM_TTL`]로 제한된다. 이러면 "확인 화면 → 실행"이 한
//! 요청 쌍으로 묶이고, 북마크·뒤로가기 재제출·같은 화면의 중복 제출이 전부 두 번째 시도부터
//! 거부된다. 이건 두 번째 **인증 요소**가 아니다 — 이미 인증된 세션(마스터 토큰 쿠키) 위에서
//! "이 특정 확인이 이 특정 실행과 짝인가"만 증명하는 **바인딩/재생 방지 논스**다. 그래서
//! 새로운 지식을 요구하지 않아도 값을 갖는다: 막는 대상이 "인증되지 않은 제3자"가 아니라
//! "이미 인증된 세션의 실수/중복 제출"이기 때문이다(모듈 헤더 "한계" 참조 — 인증되지 않은
//! 제3자를 막는 것은 여전히 [`crate::web::auth::require_auth`]의 일이다).
//!

//! ## 설계 결정 3 — 이 토큰에는 CSPRNG가 필요 없다
//! (처음 쓸 때의 근거는 "이 크레이트에 직접 의존으로 선언된 난수 crate가 없다"였다. 그
//! 사정은 바뀌었다 — [`crate::web::auth`]가 세션 ID를 만들려고 `getrandom`을 직접 의존으로
//! 올렸다. 그래도 **결론은 그대로다**: 여기서 CSPRNG를 쓸 이유가 없다.)
//!
//! 이 토큰은 애초에 암호학적 예측 불가능성이 **필요하지 않다** — 위 설계 결정 2가 설명한
//! 대로, 이 토큰이 막는 것은 "인증되지 않은 제3자의 추측"이 아니라 "이미 인증된 세션의
//! 재생/중복 제출"이다. 이 라우트들은
//! 전부 [`crate::web::auth::require_auth`] 뒤에 있으므로, 토큰을 맞혀야 얻는 것이 없는
//! 사람(인증 안 된 요청)은 애초에 이 핸들러에 도달하지 못하고, 토큰을 맞히지 않아도 이미
//! 다 가진 사람(같은 세션 쿠키를 가진 사람)은 자기 확인 화면을 새로 하나 열면 그만이다.
//! 그래서 필요한 성질은 "충돌하지 않는 고유값"뿐이고, 이미 이 크레이트가 잡 ID로 쓰는
//! [`uuid::Uuid::now_v7`](Cargo.toml의 `uuid = { version = "1", features = ["v7"] }`,
//! `src/web/job/spec.rs` 테스트가 같은 API를 쓴다)가 정확히 그 성질을 준다 — 새 의존성이
//! 없다. 토큰 값 자체는 시크릿으로 취급하지 않는다(감사 로그·에러 메시지 어디에도 원문을
//! 싣지 않지만, 이건 "새는 걱정" 때문이 아니라 "실을 이유가 없기" 때문이다).
//!
//! ## 설계 결정 4 — 거부된 확인도 감사에 남긴다
//! `fix-serve-core`가 config 경로에서 내린 판단은 "`apply` 전에 400/404로 끊기는 검증
//! 실패는 기록하지 않는다 — 실행된 것이 없고, 기록하면 쓰레기 POST로 append-only 로그를
//! 부풀리는 길이 열린다"였다(`routes::config::save`/`delete_submit`가 그 판단을 그대로
//! 따른다). **이 모듈은 프로파일명·토큰 불일치는 그 판단과 다르게 간다 — 기록한다**
//! (`.reject` 접미가 붙은 별도 action으로, [`GuardRejection::reason_tag`]와 함께).
//!
//! 다르게 가는 근거: `job::spec::JobCommand::is_destructive` doc이 이미 "과다 기록은 안전한
//! 방향의 실패다 — 'prod 복구를 누군가 시도했다'는 기록은 dry-run이었어도 감사관이 보고
//! 싶은 사실"이라고 못박았다. 파괴적 작업의 확인 실패는 그 연장선이다 — 운영자가 나중에
//! "누가 이 대상에 복구/삭제를 시도했다가 이름을 잘못 쳤나, 누가 만료된 확인을 다시
//! 눌렀나"를 되짚고 싶을 수 있다. config 경로와 위험 등급이 다르다는 것도 근거다 — config
//! 저장 실패는 최악의 경우 문법 오류이고, 이 경로의 확인 실패는 **막 되돌릴 수 없는 작업
//! 직전**이었다는 뜻이다. "쓰레기 POST가 로그를 부풀린다"는 걱정도 이 경로에는 덜
//! 들어맞는다 — 이 콘솔의 모든 라우트는 이미 [`crate::web::auth::require_auth`] 뒤에 있어,
//! 여기 도달하려면 이미 인증된 세션이 필요하다(인증 없는 제3자가 무제한으로 두드릴 수 있는
//! 표면이 아니다).
//!
//! 반대로 **출처(Origin) 불일치는 기록하지 않는다** — [`ConfirmError::ForeignOrigin`]은
//! `routes::config::reject_foreign_origin`과 같은 판단을 따른다. 출처 검사 실패는 대개
//! 운영자가 실제로 무언가를 "시도"한 것이 아니라, 다른 탭에 열린 적대적 페이지가 운영자
//! 모르게 브라우저에 쏘아 보낸 위조 요청이 튕겨난 것이다(CSRF 시나리오 자체가 그렇다 —
//! [`crate::web::auth`] 모듈 헤더 참조). "누가 시도했다"는 감사 기록이 실제로는 아무도
//! 시도하지 않은 일에 사람 이름을 붙이는 셈이 되므로, 이 경우는 기록하지 않는 원래 판단을
//! 유지한다.
//!
//! 기록은 **최선 노력**이다 — 거부된 확인은 애초에 게이트할 실행이 없으므로, 그 기록
//! 자체가 실패해도(디스크 풀 등) 거부 응답을 막지 않는다(경고만 남긴다). 반면 **승인된**
//! 확인의 감사 기록(=[`AuditLog::gate`])은 여전히 fail-closed다 — 그 실패는
//! [`ConfirmError::Audit`]로 전파되어 실행 자체를 막는다(모듈 핵심 불변식, 아래 참조).
//!
//! ## 핵심 불변식 — [`AuditReceipt`] 없이는 [`ConfirmedGate`]가 생기지 않는다
//! [`DestructiveGuard::confirm_and_gate`]는 [`AuditLog::gate`]가 append에 성공했을 때만
//! [`ConfirmedGate`]를 돌려준다(t8의 게이트 불변식을 그대로 통과시킨다 — 이 모듈은 그
//! 불변식을 **약화하지 않는다**, 확인 단계를 그 앞에 하나 더 얹을 뿐이다). 호출부(t23~25)는
//! `receipt`를 [`crate::web::job::JobRunner::spawn_destructive`]에 값으로 넘기면 된다.
//!
//! ## 설계 결정 5 — 명시적 덮어쓰기 토글
//! [`DestructiveRequest::overwrite`]가 `Some(...)`이면 화면에 별도 체크박스가 뜨고, 기본은
//! 꺼짐이다(CLI의 `--force`/`--drop-target`이 기본으로 없는 것과 같다). 체크 여부는
//! [`ConfirmSubmission::allow_overwrite`]로 이 모듈에 들어와 [`ConfirmedGate::allow_overwrite`]
//! 로 그대로 나간다 — **이 값을 argv에 반영하는 것은 호출부의 몫이다**(이 모듈은
//! `job::spec::JobFlag`를 모른다 — restore/prune/migrate가 이 토글을 각자 어떤 플래그
//! 조합으로 옮길지는 명령마다 다르다). 호출부는 반드시 이 값을 보고 분기해야 하며, 기본값
//! (`false`)을 놓치면 안 된다는 것을 타입에 이름으로 새겨 두었다.
//!
//! ## API 모양
//! 1. `GET` 라우트가 [`DestructiveGuard::render_confirm`]으로 확인 화면을 그린다.
//! 2. `POST` 라우트가 폼을 자기 방식대로 파싱해([`FIELD_CONFIRM_TOKEN`]·
//!    [`FIELD_CONFIRM_NAME`]·[`FIELD_ALLOW_OVERWRITE`] 세 필드) [`ConfirmSubmission`]을
//!    만들고, [`DestructiveGuard::confirm_and_gate`]를 부른다. 이 모듈은
//!    `application/x-www-form-urlencoded`를 모른다 — restore/prune/migrate 폼은 이 세
//!    필드 말고도 명령별 필드(`--at`, `--only`, `--db` 등)를 함께 파싱해야 하므로, 본문
//!    파싱은 호출부 책임으로 남겨 이 모듈이 그 어휘를 알 필요가 없게 한다
//!    (`routes::config`가 자기 폼 파싱을 스스로 갖는 것과 같은 이유).
//! 3. `confirm_and_gate`는 출처 검사([`crate::web::auth::verify_same_origin`])까지 이
//!    한 곳에서 해 준다 — 세 호출부가 각자 잊을 수 있는 자리를 하나로 줄인다
//!    (`routes::config`의 `reject_foreign_origin`과 같은 필요성, 여기서는 공용화했다).
//!
//! ## 한계
//! - **확인 화면만 열고 방치하면** 그 토큰은 [`CONFIRM_TTL`]이 지날 때까지 메모리에 남는다.
//!   청소는 다음 발급/제출 시점에 게으르게(lazy) 이루어진다([`DestructiveGuard::sweep_expired`]) —
//!   백그라운드 청소 태스크를 두려면 `server.rs`의 기동 배선이 필요한데 그건 이 태스크
//!   범위 밖이다(리더 소관). 이 한계가 문제가 되려면 인증된 세션이 확인 화면을 대량으로
//!   열고 버려야 하는데, 그 세션은 이미 파괴적 작업을 실제로 실행할 권한이 있으므로 얻는
//!   것이 없는 공격이다.
//! - 토큰은 [`Uuid::now_v7`]로 만든다 — 시간 정렬 가능한 값이라 **순서를 추측할 수 있다**
//!   (뒷자리가 완전 무작위가 아닐 수 있다). 위에서 설명했듯 이 모듈에서는 문제가 되지
//!   않지만, 이 값을 다른 문맥(예: 인증되지 않은 제3자를 막아야 하는 자리)에 재사용하면
//!   안 된다.
//! - 이 모듈은 `crate::engine`·`crate::storage`·`crypto`를 참조하지 않는다([`crate::web`]
//!   모듈 헤더의 최상위 불변식) — 여기 있는 것은 확인·감사 배선뿐이고, 실제 자식 프로세스
//!   실행은 호출부가 [`crate::web::job::JobRunner`]로 한다.

use std::collections::HashMap;
use std::time::Duration;

use axum::http::HeaderMap;
use maud::{html, Markup};
use tokio::sync::Mutex;
use tokio::time::Instant;
use uuid::Uuid;

use crate::error::XBackupError;
use crate::i18n::Lang;
use crate::web::audit::{AuditEvent, AuditLog, AuditOutcome, AuditReceipt};
use crate::web::auth::{verify_same_origin, OriginProblem};
use crate::web::job::ProfileName;
use crate::web::view::components::{self, Level};

/// 확인 토큰을 싣는 hidden 폼 필드 이름.
pub const FIELD_CONFIRM_TOKEN: &str = "confirm_token";
/// 타이핑한 프로파일명을 싣는 폼 필드 이름.
pub const FIELD_CONFIRM_NAME: &str = "confirm_name";
/// 덮어쓰기 토글 체크박스의 폼 필드 이름.
pub const FIELD_ALLOW_OVERWRITE: &str = "allow_overwrite";

/// 확인 화면이 발급한 토큰이 유효한 최대 시간.
///
/// ## 5분의 근거
/// 사람이 확인 화면을 읽고("무엇이 어디에 대해 일어나는지"), 프로파일명을 정확히
/// 타이핑하고, 덮어쓰기 토글을 판단하기에 충분해야 한다 — 너무 짧으면 화면을 읽는 동안
/// 시간이 지나 "만료됐습니다, 다시 확인하세요"가 정상 사용을 방해한다. 동시에 너무 길면
/// 확인 화면을 열어 둔 채 자리를 비운 세션이 오래 유효한 "지금 눌러도 되는" 실행 경로로
/// 남는다. [`crate::web::auth::FAILURE_DECAY_PERIOD`](60초)보다 훨씬 여유 있게 잡은 이유는
/// 그 값은 "실패 카운터가 잊히는" 방향(짧을수록 관대)이고 이 값은 "확인이 살아있는" 방향
/// (짧을수록 엄격)이라 성격이 반대이기 때문이다.
const CONFIRM_TTL: Duration = Duration::from_secs(5 * 60);

/// 확인 화면 하나가 발급하는 일회용 토큰 — 설계 결정 2·3(모듈 헤더) 참조.
///
/// [`Uuid::now_v7`]로 만든다. 이 값 자체를 시크릿으로 다루지 않는다 — 감사 로그·에러
/// 메시지 어디에도 원문을 싣지 않지만, 그건 "새면 위험해서"가 아니라 "실을 이유가 없어서"
/// 다(모듈 헤더 설계 결정 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct ConfirmToken(Uuid);

impl ConfirmToken {
    /// 새 토큰을 발급한다.
    fn issue() -> Self {
        Self(Uuid::now_v7())
    }

    /// 폼에서 받은 원문 문자열을 토큰으로 해석한다. UUID 문법이 아니면(길이·형식 무관하게
    /// 어떤 적대적 문자열이 와도) `None`이다 — 패닉하지 않는다.
    fn parse(raw: &str) -> Option<Self> {
        Uuid::parse_str(raw.trim()).ok().map(Self)
    }
}

impl std::fmt::Display for ConfirmToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

/// 토큰 하나가 무엇을 확인시키기 위해 발급됐는지 — 서버 메모리에만 산다.
///
/// [`ConfirmToken::issue`]가 만들 때 [`DestructiveTarget`]을 그대로 복사해 둔다. 제출
/// 시점에 호출부가 주장하는 action/target과 이 값을 대조해야, 한 라우트에서 받은 토큰이
/// 다른 라우트(다른 명령·다른 프로파일)의 실행에 잘못 쓰이는 경로를 막을 수 있다(아래
/// [`DestructiveGuard::confirm_and_gate`] doc의 "action/target 불일치" 참조).
#[derive(Debug, Clone)]
struct PendingConfirmation {
    action: &'static str,
    target: String,
    issued_at: Instant,
}

/// 파괴적 작업 하나를 "무엇에 대한 것인가"로 식별한다 — 확인 화면 발급과 제출 검증이
/// 공유하는 최소 단위.
///
/// `action`은 감사 로그 `action` 필드로 그대로 쓰인다 — `job::spec::JobCommand::audit_action`
/// 이 내놓는 값(예: `"restore.run"`)을 그대로 넘기면 된다(호출부가 두 어휘를 만들 필요가
/// 없다).
#[derive(Debug, Clone, Copy)]
pub struct DestructiveTarget<'a> {
    /// 감사 로그 `action` 필드 값. `job::spec::JobCommand::audit_action()`과 같은 값을 쓴다.
    pub action: &'static str,
    /// 대상 프로파일 — 확인 화면의 이름 재입력 검증 기준이자 감사 로그 `target` 필드 값.
    pub target: &'a ProfileName,
}

/// 덮어쓰기 토글 하나의 화면 문구 — CLI `--force`/`--drop-target`에 대응.
pub struct OverwriteOption {
    /// 체크박스 옆에 붙는 한 줄 설명(무엇을 덮어쓰는지).
    pub label: String,
    /// 그 아래 작은 안내 문구(왜 기본이 꺼져 있는지 등).
    pub hint: String,
}

/// 확인 화면을 그리는 데 필요한 전부 — 호출부(t23~25)가 "무엇을 확인시킬지"로 채운다.
pub struct DestructiveRequest<'a> {
    /// 무엇을 확인시키는가(action + target).
    pub what: DestructiveTarget<'a>,
    /// 배너 헤드라인 — "무슨 일이 일어나는가"(설명 문장, 언어는 호출부가 이미 고른 뒤 넘긴다).
    pub headline: String,
    /// 되돌릴 수 없다는 사실을 명시하는 문장 — **필수**다(`Option`이 아니다). 이 화면의
    /// 존재 이유가 "되돌릴 수 없음을 말하는 것"이므로, 이 필드를 비워 둘 수 있게 두면
    /// 호출부가 실수로 그 사실을 빠뜨릴 수 있다.
    pub irreversible_notice: String,
    /// 라벨:값 요약 줄들(예: `("command", "restore")`, `("recovery point", "…")`).
    pub summary: Vec<(&'static str, String)>,
    /// 덮어쓰기 토글. `None`이면 이 작업엔 그 개념이 없다는 뜻이고 화면에 체크박스가
    /// 아예 뜨지 않는다.
    pub overwrite: Option<OverwriteOption>,
    /// `POST` 제출 경로.
    pub submit_path: &'a str,
    /// 제출 버튼 문구 — 무엇이 일어날지 말해야 한다("Submit" 같은 범용 문구 금지, 이
    /// 화면의 다른 부분과 같은 원칙).
    pub submit_label: String,
    /// "취소" 링크가 가리키는 경로.
    pub cancel_href: &'a str,
    /// 확인 폼에 함께 실어 보낼 호출부 전용 hidden 필드 `(이름, 값)`.
    ///
    /// ## 왜 이 자리가 필요한가
    /// 확인 폼을 **이 모듈이 그리므로**, 호출부가 실행에 필요한 값(prune의 프로파일·보존
    /// 기준·계획 지문, restore의 백업 id·복구 시점 등)을 그 폼 안에 넣을 방법이 달리 없다.
    /// 대안은 `submit_path`에 쿼리로 붙이는 것인데, 그러면 그 값들이 브라우저 히스토리·
    /// 리퍼러·프록시 로그에 남는다 — `routes::peek` 헤더가 GET 쿼리를 피한 것과 같은 이유다.
    ///
    /// ## 이 모듈은 값의 뜻을 모른다
    /// 여기 담긴 이름·값은 이 모듈에 아무 의미가 없다 — 그대로 hidden 필드로 그려 돌려보낼
    /// 뿐이고, 되돌아온 값을 **다시 검증하는 것은 호출부의 몫**이다. 특히 이 값들은
    /// 확인 토큰이 보호하지 않는다(토큰은 action+target에만 묶인다) — 사용자가 폼을 고쳐
    /// 보낼 수 있다고 가정하고 다뤄야 한다. prune이 지문을 되받아 **다시 계산해 비교**하는
    /// 것이 그 예다.
    ///
    /// [`FIELD_CONFIRM_TOKEN`]·[`FIELD_CONFIRM_NAME`]·[`FIELD_ALLOW_OVERWRITE`]와 같은
    /// 이름을 넣으면 이 모듈이 그리는 필드와 충돌한다 — 그 셋은 예약된 이름이다.
    pub extra_hidden: Vec<(String, String)>,
}

/// [`DestructiveGuard::confirm_and_gate`] 호출 하나에 필요한 문맥.
pub struct DestructiveContext<'a> {
    /// 무엇을 확인하는가 — [`DestructiveRequest::what`]과 **같은 값**이어야 한다(다르면
    /// [`GuardRejection::ActionOrTargetMismatch`]로 거부된다).
    pub what: DestructiveTarget<'a>,
    /// 감사 로그 `actor` 필드 값. 이 콘솔에는 세션 토큰 하나뿐이라 사용자별 신원이 없다
    /// (`routes::config::AUDIT_ACTOR`와 같은 사정 — 통상 `"web"`).
    pub actor: &'a str,
}

/// 제출된 폼에서 뽑아야 하는 세 값 — 이 모듈은 `application/x-www-form-urlencoded`를
/// 모르므로(모듈 헤더 "API 모양"), 호출부가 자기 방식으로 파싱해 채워 넣는다.
pub struct ConfirmSubmission<'a> {
    /// [`FIELD_CONFIRM_TOKEN`] 필드의 원문(없으면 `None`).
    pub token: Option<&'a str>,
    /// [`FIELD_CONFIRM_NAME`] 필드의 원문 — **자르지 않은 그대로** 넘겨야 한다. 대상
    /// 프로파일명과 바이트 단위로 정확히 같아야 통과한다(`routes::config::delete_submit`와
    /// 같은 엄격함 — 공백을 관대하게 접으면 "이름을 타이핑했다"는 사실 자체가 약해진다).
    pub typed_name: &'a str,
    /// [`FIELD_ALLOW_OVERWRITE`] 체크박스가 체크됐는지. 이 모듈은 체크박스 부재/빈 값
    /// 판정을 하지 않는다 — 호출부가 이미 판정해 불리언으로 넘긴다(HTML 체크박스는
    /// 체크 해제 시 필드 자체가 전송되지 않는 것이 표준 동작이므로, 판정 로직은 폼 파싱과
    /// 함께 있는 편이 자연스럽다).
    pub allow_overwrite: bool,
}

/// 확인이 거부된 이유 — 프로파일명·토큰 불일치 계열.
///
/// `#[non_exhaustive]`인 이유는 [`crate::web::audit::AuditOutcome`]과 같다 — 후속 태스크가
/// 이 확인 게이트에 새 거부 사유(예: 동시 제출 상한)를 추가할 수 있게 열어 둔다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GuardRejection {
    /// 토큰이 없거나, UUID 문법이 아니거나, 서버가 모르거나(이미 소비됐거나 만료됐거나
    /// 애초에 발급한 적이 없는 값), 만료됐다. 이 넷을 하나로 묶는 이유는 사용자에게 줄 수
    /// 있는 조치가 하나("확인 화면을 다시 여세요")뿐이기 때문이다 — 세분화해 알려주면
    /// 공격자에게 "어느 토큰이 왜 실패했는지"라는 힌트만 늘어난다.
    InvalidToken,
    /// 토큰은 유효하지만, 이 토큰이 발급될 때 묶인 action/target이 지금 이 요청이 주장하는
    /// 것과 다르다 — 한 라우트에서 받은 토큰을 다른 라우트(다른 명령·다른 프로파일)의
    /// 실행에 잘못 넘긴 경로다(모듈 헤더 [`PendingConfirmation`] doc 참조).
    ActionOrTargetMismatch,
    /// 타이핑한 이름이 대상 프로파일명과 정확히 다르다.
    NameMismatch,
}

impl GuardRejection {
    /// 감사 로그 args에 남기는 짧은 사유 태그 — grep 가능한 고정 어휘([`crate::web::audit`]
    /// 모듈 헤더의 같은 이유).
    fn reason_tag(self) -> &'static str {
        match self {
            Self::InvalidToken => "invalid_token",
            Self::ActionOrTargetMismatch => "target_mismatch",
            Self::NameMismatch => "name_mismatch",
        }
    }

    /// 화면에 보여주는 설명 문장.
    pub fn explain(self, lang: Lang) -> String {
        match self {
            Self::InvalidToken => lang
                .sel(
                    "This confirmation link is invalid or has expired — nothing was done. Reopen the confirmation screen and try again.",
                    "이 확인 링크가 유효하지 않거나 만료되었습니다 — 아무 작업도 하지 않았습니다. 확인 화면을 다시 열어 시도하세요.",
                )
                .to_string(),
            Self::ActionOrTargetMismatch => lang
                .sel(
                    "This confirmation does not match the requested operation — nothing was done. Reopen the confirmation screen and try again.",
                    "이 확인이 요청한 작업과 일치하지 않습니다 — 아무 작업도 하지 않았습니다. 확인 화면을 다시 열어 시도하세요.",
                )
                .to_string(),
            Self::NameMismatch => lang
                .sel(
                    "The confirmation text did not match the profile name — nothing was done. Type the name exactly as shown.",
                    "확인 입력이 프로파일 이름과 다릅니다 — 아무 작업도 하지 않았습니다. 표시된 이름을 그대로 입력하세요.",
                )
                .to_string(),
        }
    }
}

/// [`DestructiveGuard::confirm_and_gate`]가 실패할 수 있는 두 층 — "확인 자체가 거부됨"과
/// "확인은 통과했는데 감사 로그를 쓸 수 없어 실행을 막음"은 원인도, 화면에 줄 상태 코드도
/// 다르다(`routes::config::apply_and_render`가 400 계열과 503을 가르는 것과 같은 판단).
#[derive(Debug)]
pub enum ConfirmError {
    /// 상태 변경 요청의 출처가 이 콘솔이 아니다([`crate::web::auth::verify_same_origin`]).
    /// **감사에 남기지 않는다** — 모듈 헤더 설계 결정 4의 "출처 불일치는 기록하지 않는다"
    /// 참조.
    ForeignOrigin(OriginProblem),
    /// 프로파일명·토큰이 맞지 않는다. **감사에 최선 노력으로 남는다**(모듈 헤더 설계
    /// 결정 4).
    Rejected(GuardRejection),
    /// 확인은 통과했지만 [`AuditLog::gate`]가 append에 실패했다 — 실행은 시작되지 않는다
    /// (모듈 핵심 불변식).
    Audit(XBackupError),
}

impl ConfirmError {
    /// 화면에 보여주는 설명 문장.
    pub fn explain(&self, lang: Lang) -> String {
        match self {
            Self::ForeignOrigin(problem) => problem.explain(lang),
            Self::Rejected(rejection) => rejection.explain(lang),
            Self::Audit(error) => format!(
                "{} {error}",
                lang.sel(
                    "The confirmation was valid, but the audit log could not be written, so nothing was run:",
                    "확인은 유효했지만 감사 로그에 기록할 수 없어 아무것도 실행하지 않았습니다:",
                )
            ),
        }
    }
}

/// 통과한 확인의 결과 — [`AuditReceipt`]를 담고 있으므로, 이 값을 손에 쥐었다는 것 자체가
/// "확인 화면 → 서버 측 재검증 → 감사 게이트"를 전부 지났다는 증거다.
///
/// `receipt`는 `Clone`이 아니다([`AuditReceipt`] 자체가 그렇다) — 이 값을 소비해야만
/// `job::JobRunner::spawn_destructive`를 호출할 수 있고, 그 소비는 한 번뿐이다.
#[must_use = "ConfirmedGate를 버리면 파괴적 작업을 실행할 방법이 없어집니다 — spawn_destructive()로 바로 소비하세요"]
pub struct ConfirmedGate {
    /// 실행 직전 감사 게이트를 통과했다는 증표. `job::JobRunner::spawn_destructive`에
    /// 값으로 넘긴다.
    pub receipt: AuditReceipt,
    /// 덮어쓰기 토글이 체크됐는지 — **argv에 반영하는 것은 호출부의 몫이다**(모듈 헤더
    /// 설계 결정 5). 기본은 `false`다.
    pub allow_overwrite: bool,
}

/// 파괴적 작업 확인 상태 — 프로세스 메모리에만 산다.
///
/// [`crate::web::auth::AuthState`]의 실패 카운터와 같은 성격이다: 재시작하면 초기화되고,
/// 영속시킬 만큼 중요한 상태가 아니다(살아있는 확인 화면은 재시작 시점에 어차피 다시
/// 열어야 정상이다 — 재시작 자체가 "지금 이 세션 상태를 믿지 말라"는 신호와 같다).
///
/// `ServeConfig`에 `Arc<DestructiveGuard>` 필드로 얹혀 라우트 핸들러가 공유한다(이 파일은
/// `ServeConfig`를 건드리지 않는다 — 배선은 리더 소관, 모듈 헤더 "API 모양" 참조).
#[derive(Debug, Default)]
pub struct DestructiveGuard {
    pending: Mutex<HashMap<ConfirmToken, PendingConfirmation>>,
}

impl DestructiveGuard {
    /// 빈 상태로 가드를 만든다.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// 테스트 전용 — 지금 살아있는(만료되지 않았을 수도 있는) 확인 항목 수.
    #[cfg(test)]
    async fn pending_len(&self) -> usize {
        self.pending.lock().await.len()
    }

    /// 테스트 전용 — 대기 항목을 심고 `(토큰, 발급 시각)`을 함께 돌려준다.
    ///
    /// 발급 시각은 항상 [`Instant::now`]로 잡는다(과거로 되돌리지 않는다) — `Instant`는
    /// 뺄셈이 그 시각보다 앞선 기준으로 내려가면 패닉할 수 있으므로, "만료됐다"는 상황은
    /// 이 시각에서 **거꾸로** 만들지 않고 검사 쪽의 `now`를 **앞으로**(덧셈으로) 밀어서
    /// 만든다(`is_expired` 테스트, `check_and_consume`/`sweep_expired_at`가 `now`를 인자로
    /// 받는 이유와 같다). 이 방식은 `tokio::time::advance`(`test-util` feature 필요,
    /// `Cargo.toml`은 이 태스크 범위 밖)를 쓰지 않고도 실제 시계를 조금도 흔들지 않는다.
    #[cfg(test)]
    async fn issue_for_test(&self, action: &'static str, target: &str) -> (ConfirmToken, Instant) {
        let token = ConfirmToken::issue();
        let issued_at = Instant::now();
        self.pending.lock().await.insert(
            token,
            PendingConfirmation {
                action,
                target: target.to_string(),
                issued_at,
            },
        );
        (token, issued_at)
    }

    /// 확인 화면을 그리고, 그 화면에 묶인 새 토큰을 발급해 보관한다.
    ///
    /// 렌더링 자체는 아무것도 감사에 남기지 않는다 — 화면을 보는 것은 아직 "시도"가
    /// 아니다(모듈 헤더 설계 결정 4는 **제출**에 대해서만 말한다).
    pub async fn render_confirm(&self, lang: Lang, request: DestructiveRequest<'_>) -> Markup {
        self.sweep_expired().await;

        let token = ConfirmToken::issue();
        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                token,
                PendingConfirmation {
                    action: request.what.action,
                    target: request.what.target.as_str().to_string(),
                    issued_at: Instant::now(),
                },
            );
        }

        html! {
            (components::verdict_banner(Level::Fail, &request.headline, Some(&request.irreversible_notice)))
            (components::panel(
                html! { h3 class="panel__title mono" { (request.what.target.as_str()) } },
                html! {
                    (components::meta_list(&request.summary))
                    form method="post" action=(request.submit_path) class="cfg-form" {
                        input type="hidden" name=(FIELD_CONFIRM_TOKEN) value=(token.to_string());
                        @for (name, value) in &request.extra_hidden {
                            input type="hidden" name=(name) value=(value);
                        }
                        div class="field" {
                            label for="f-confirm-name" class="field__label mono" { (FIELD_CONFIRM_NAME) }
                            input type="text" id="f-confirm-name" name=(FIELD_CONFIRM_NAME)
                                autocomplete="off" required;
                            p class="field__hint" {
                                (lang.sel(
                                    "Type the profile name exactly to confirm.",
                                    "확인을 위해 프로파일 이름을 그대로 입력하세요.",
                                ))
                            }
                        }
                        @if let Some(overwrite) = &request.overwrite {
                            div class="field" {
                                label for="f-allow-overwrite" {
                                    input type="checkbox" id="f-allow-overwrite"
                                        name=(FIELD_ALLOW_OVERWRITE) value="1";
                                    " " (overwrite.label)
                                }
                                p class="field__hint" { (overwrite.hint) }
                            }
                        }
                        p class="actions" {
                            button type="submit" { (request.submit_label) }
                            " "
                            a href=(request.cancel_href) { (lang.sel("Cancel", "취소")) }
                        }
                    }
                },
            ))
        }
    }

    /// 제출을 검증하고, 통과하면 감사 게이트까지 지나 [`ConfirmedGate`]를 돌려준다.
    ///
    /// ## 검사 순서 — 왜 이 순서인가
    /// 1. **출처**([`verify_same_origin`]) — 위조된 요청이 뒤 단계에서 감사 로그에 줄을
    ///    남기게 하면 그 자체가 공격이 된다(`routes::config`의 같은 순서 근거).
    /// 2. **토큰**(존재·문법·조회·만료) — 이 시점에 토큰을 **소비**한다(성공하든 이후
    ///    단계에서 거부되든). 재생/중복 제출을 "한 번의 시도"로 좁히는 지점이 여기다.
    /// 3. **action/target 일치** — 이 토큰이 애초에 이 요청을 위해 발급됐는지.
    /// 4. **이름 재입력 일치** — 사람이 실제로 그 이름을 정확히 타이핑했는지.
    /// 5. **감사 게이트**([`AuditLog::gate`]) — 여기서 성공해야만 [`ConfirmedGate`]가
    ///    생긴다.
    ///
    /// `masked_args_for`는 `allow_overwrite`가 정해진 **뒤에** 호출된다 — 감사 로그에 남는
    /// argv 표현이 실제로 실행될 argv(덮어쓰기 플래그 포함 여부)와 어긋나지 않아야 하기
    /// 때문이다. 호출부는 이 클로저 안에서 자기 `JobSpec`을 완성하고
    /// `job::spec::JobSpec::masked_args`를 불러 돌려주면 된다.
    pub async fn confirm_and_gate<F>(
        &self,
        audit: &AuditLog,
        headers: &HeaderMap,
        context: DestructiveContext<'_>,
        submission: ConfirmSubmission<'_>,
        masked_args_for: F,
    ) -> std::result::Result<ConfirmedGate, ConfirmError>
    where
        F: FnOnce(bool) -> Vec<String>,
    {
        if let Err(problem) = verify_same_origin(headers) {
            tracing::warn!(
                action = context.what.action,
                target = context.what.target.as_str(),
                reason = ?problem,
                "출처를 확인할 수 없어 파괴적 작업 확인 요청을 거부했습니다"
            );
            return Err(ConfirmError::ForeignOrigin(problem));
        }

        self.sweep_expired().await;

        if let Some(rejection) = self
            .check_and_consume(&context, &submission, Instant::now())
            .await
        {
            self.record_rejection(audit, &context, rejection).await;
            return Err(ConfirmError::Rejected(rejection));
        }

        let masked_args = masked_args_for(submission.allow_overwrite);
        let receipt = audit
            .gate(
                context.actor,
                context.what.action,
                context.what.target.as_str(),
                &masked_args,
            )
            .await
            .map_err(ConfirmError::Audit)?;

        Ok(ConfirmedGate {
            receipt,
            allow_overwrite: submission.allow_overwrite,
        })
    }

    /// 토큰을 조회·소비하고, action/target·이름 일치를 검사한다.
    ///
    /// 토큰은 **여기서 무조건 소비된다**(찾았으면 맵에서 제거) — 그 뒤의 action/target·
    /// 이름 검사가 실패하더라도 되돌리지 않는다. 그러지 않으면 같은 토큰으로 이름을 여러
    /// 번 틀리게 제출해 보는 것이 허용되고, "확인 화면 → 실행이 한 번만 성립한다"는 계약이
    /// 깨진다.
    ///
    /// `now`을 인자로 받는 이유는 [`crate::web::auth::AuthState::note_failure`]가 `now`를
    /// 받는 이유와 같다 — 이 값이 있어야 만료 판정을 실제 시계 없이(테스트에서
    /// [`Instant::now`] 대신 합성한 미래 시각을 넘겨) 검증할 수 있다. `tokio::time::advance`
    /// (`test-util` feature 필요, `Cargo.toml`은 이 태스크 범위 밖)를 쓰지 않고도 TTL
    /// 경계를 고정하는 방법이다.
    async fn check_and_consume(
        &self,
        context: &DestructiveContext<'_>,
        submission: &ConfirmSubmission<'_>,
        now: Instant,
    ) -> Option<GuardRejection> {
        // 이 함수의 반환 타입 자체가 `Option<GuardRejection>`이라 `?`(Option 조기 반환)를
        // 그대로 쓰면 안 된다 — `?`는 `None`이면 함수 밖으로 `None`을 돌려주는데, 이
        // 함수에서 `None`은 "거부 아님"(통과)이다. "토큰이 없다"를 `?`로 처리하면 그
        // 즉시 "거부 아님"으로 뒤집혀 버린다. 그래서 각 단계를 명시적으로 매치해 반드시
        // [`GuardRejection::InvalidToken`]으로 접는다.
        let Some(raw_token) = submission.token else {
            return Some(GuardRejection::InvalidToken);
        };
        let Some(token) = ConfirmToken::parse(raw_token) else {
            return Some(GuardRejection::InvalidToken);
        };
        let entry = {
            let mut pending = self.pending.lock().await;
            pending.remove(&token)
        };
        let Some(entry) = entry else {
            return Some(GuardRejection::InvalidToken);
        };

        if is_expired(entry.issued_at, now) {
            return Some(GuardRejection::InvalidToken);
        }
        if entry.action != context.what.action || entry.target != context.what.target.as_str() {
            return Some(GuardRejection::ActionOrTargetMismatch);
        }
        if submission.typed_name != context.what.target.as_str() {
            return Some(GuardRejection::NameMismatch);
        }
        None
    }

    /// 거부된 확인을 감사 로그에 최선 노력으로 남긴다(모듈 헤더 설계 결정 4).
    ///
    /// action에 `.reject` 접미를 붙여 실제 실행 기록(`<verb>.run`)과 grep으로 분리되게
    /// 한다. 타이핑한 이름(`submission.typed_name`)은 **여기 싣지 않는다** — 적대적이거나
    /// 우연히 다른 시크릿이 섞인 문자열이 append-only 로그에 원문으로 영구히 남는 경로를
    /// 만들 이유가 없다(운영자에게 필요한 정보는 "누가 언제 무엇에 대해 실패했나"이지
    /// "무엇을 잘못 쳤나"가 아니다).
    async fn record_rejection(
        &self,
        audit: &AuditLog,
        context: &DestructiveContext<'_>,
        reason: GuardRejection,
    ) {
        let reject_action = format!("{}.reject", context.what.action);
        let args = vec![format!("reason={}", reason.reason_tag())];
        if let Err(error) = audit
            .record(AuditEvent {
                actor: context.actor,
                action: &reject_action,
                target: context.what.target.as_str(),
                args_masked: &args,
                outcome: AuditOutcome::Failure,
                exit_code: None,
            })
            .await
        {
            // 최선 노력이다 — 여기엔 게이트할 실행이 없으므로(이미 거부됐다) 기록 실패로
            // 거부 응답 자체를 막지 않는다. 다만 조용히 삼키지는 않는다.
            tracing::warn!(
                action = %reject_action,
                target = context.what.target.as_str(),
                error = %error,
                "거부된 확인 시도를 감사 로그에 남기지 못했습니다"
            );
        }
    }

    /// 만료된 대기 항목을 청소한다 — 발급/제출 시점마다 게으르게 부른다(모듈 헤더 "한계").
    async fn sweep_expired(&self) {
        self.sweep_expired_at(Instant::now()).await;
    }

    /// [`sweep_expired`](Self::sweep_expired)의 본체 — `now`를 인자로 받는 이유는
    /// [`check_and_consume`](Self::check_and_consume)와 같다(테스트에서 실제 시계를
    /// 흔들지 않고 청소를 검증하기 위함).
    async fn sweep_expired_at(&self, now: Instant) {
        let mut pending = self.pending.lock().await;
        pending.retain(|_, entry| !is_expired(entry.issued_at, now));
    }
}

/// 발급 시각 기준으로 [`CONFIRM_TTL`]이 지났는지 — 순수 함수라 시계 없이 경계값을 고정할
/// 수 있다(`crate::web::auth`의 `decay_failures`와 같은 패턴).
///
/// [`Instant::duration_since`]는 `now`가 `issued_at`보다 앞서도(이론상 있을 수 없지만)
/// 음수 대신 0으로 포화한다(Rust 1.60+) — 그래서 여기서 뺄셈이 패닉할 일이 없다.
fn is_expired(issued_at: Instant, now: Instant) -> bool {
    now.duration_since(issued_at) > CONFIRM_TTL
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::audit::AuditLog;
    use axum::http::{HeaderMap, HeaderValue};

    /// `ProfileName`을 테스트에서 간단히 만든다.
    fn profile(name: &str) -> ProfileName {
        ProfileName::parse(name, Lang::En).unwrap()
    }

    /// 같은 origin으로 보이는 헤더 — `verify_same_origin`을 통과시킨다.
    fn same_origin_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("console.example"));
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://console.example"),
        );
        headers
    }

    /// 파일에서 각 줄을 `serde_json::Value`로 파싱한다(빈 줄은 건너뜀) —
    /// `crate::web::audit`의 테스트 헬퍼와 같은 패턴.
    fn read_lines(path: &std::path::Path) -> Vec<serde_json::Value> {
        let content = std::fs::read_to_string(path).unwrap();
        content
            .lines()
            .filter(|l| !l.is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    fn sample_request<'a>(target: &'a ProfileName) -> DestructiveRequest<'a> {
        DestructiveRequest {
            what: DestructiveTarget {
                action: "restore.run",
                target,
            },
            headline: "This will overwrite the target database.".to_string(),
            irreversible_notice: "Existing data at the target cannot be recovered afterward."
                .to_string(),
            summary: vec![("command", "restore".to_string())],
            // 호출부 전용 hidden 필드가 실제로 폼에 실리는지 아래 테스트가 확인한다.
            extra_hidden: vec![("backup_id".to_string(), "bk-42".to_string())],
            overwrite: Some(OverwriteOption {
                label: "Allow overwriting existing data (--force)".to_string(),
                hint: "Off by default — matches the CLI's --force gate.".to_string(),
            }),
            submit_path: "/restore/confirmed",
            submit_label: "Restore now".to_string(),
            cancel_href: "/backup",
        }
    }

    fn sample_context<'a>(target: &'a ProfileName) -> DestructiveContext<'a> {
        DestructiveContext {
            what: DestructiveTarget {
                action: "restore.run",
                target,
            },
            actor: "web",
        }
    }

    /// 확인 화면을 그리면 새 토큰이 하나 저장된다.
    #[tokio::test]
    async fn render_confirm_stores_a_pending_token() {
        let guard = DestructiveGuard::new();
        let target = profile("prod");
        assert_eq!(guard.pending_len().await, 0);
        let _markup = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await;
        assert_eq!(guard.pending_len().await, 1);
    }

    /// 렌더된 화면에는 인라인 스타일·색 리터럴이 없고(`components` 규약), 숨김 토큰·
    /// 이름 입력 필드가 실제로 있다.
    #[tokio::test]
    async fn rendered_markup_follows_presentation_rules_and_has_required_fields() {
        let guard = DestructiveGuard::new();
        let target = profile("prod");
        let out = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await
            .into_string();
        assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
        assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
        assert!(out.contains(&format!(r#"name="{FIELD_CONFIRM_TOKEN}""#)));
        assert!(out.contains(&format!(r#"name="{FIELD_CONFIRM_NAME}""#)));
        assert!(out.contains(&format!(r#"name="{FIELD_ALLOW_OVERWRITE}""#)));
        assert!(
            out.contains(r#"data-level="fail""#),
            "레벨 배지 누락: {out}"
        );
        assert!(out.contains("cannot be recovered"));
    }

    /// 적대적 문자열(헤드라인·요약)은 이스케이프된다 — `components`가 이미 보장하지만
    /// 이 모듈이 그 보장을 실제로 쓰는지 회귀로 고정한다.
    #[tokio::test]
    async fn hostile_strings_in_request_are_escaped() {
        let guard = DestructiveGuard::new();
        let target = profile("prod");
        let mut request = sample_request(&target);
        request.headline = r#"<img src=x onerror="alert(1)">"#.to_string();
        let out = guard.render_confirm(Lang::En, request).await.into_string();
        assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
        assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
    }

    /// 토큰 없는 제출은 거부되고, 승인된 게이트(receipt)를 만들 방법이 없다.
    #[tokio::test]
    async fn missing_token_is_rejected() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let result = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                ConfirmSubmission {
                    token: None,
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;

        assert!(matches!(
            result.err(),
            Some(ConfirmError::Rejected(GuardRejection::InvalidToken))
        ));
    }

    /// UUID 문법이 아닌 토큰(매우 긴 문자열 포함)은 패닉 없이 거부된다.
    #[tokio::test]
    async fn malformed_or_huge_token_is_rejected_without_panicking() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");
        let huge = "a".repeat(100_000);

        for bad_token in ["", "not-a-uuid", "'; DROP TABLE x;--", huge.as_str()] {
            let result = guard
                .confirm_and_gate(
                    &audit,
                    &same_origin_headers(),
                    sample_context(&target),
                    ConfirmSubmission {
                        token: Some(bad_token),
                        typed_name: "prod",
                        allow_overwrite: false,
                    },
                    |_| Vec::new(),
                )
                .await;
            assert!(matches!(
                result.err(),
                Some(ConfirmError::Rejected(GuardRejection::InvalidToken))
            ));
        }
    }

    /// 정상 흐름: 화면을 그려 토큰을 받고, 올바른 이름·토큰으로 제출하면 감사 게이트를
    /// 지난 receipt를 얻는다 — 그 receipt가 있어야만 실행할 수 있는 함수 모양으로
    /// 확인한다(`crate::web::audit`의 같은 패턴).
    #[tokio::test]
    async fn successful_confirmation_yields_a_receipt_before_any_execution() {
        fn run_destructive_op(_receipt: AuditReceipt, label: &str) -> String {
            format!("executed: {label}")
        }

        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let markup = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await
            .into_string();
        let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

        let confirmed = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                ConfirmSubmission {
                    token: Some(&token),
                    typed_name: "prod",
                    allow_overwrite: true,
                },
                |allow_overwrite| vec![format!("force={allow_overwrite}")],
            )
            .await
            .expect("올바른 확인은 통과해야 함");

        assert!(confirmed.allow_overwrite);
        let result = run_destructive_op(confirmed.receipt, "prod");
        assert_eq!(result, "executed: prod");

        // 감사 로그에는 "requested" 한 줄만 남아 있어야 한다(실행 완료 기록은 호출부 몫).
        let lines = read_lines(audit.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["outcome"], "requested");
        assert_eq!(lines[0]["action"], "restore.run");
        assert_eq!(lines[0]["target"], "prod");
        assert_eq!(lines[0]["args_masked"], serde_json::json!(["force=true"]));
    }

    /// 확인 화면 → 실행은 한 번만 성립한다 — 같은 토큰으로 두 번째 제출은 거부된다
    /// (재생·중복 제출 차단).
    #[tokio::test]
    async fn same_token_cannot_be_submitted_twice() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let markup = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await
            .into_string();
        let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

        let submission = || ConfirmSubmission {
            token: Some(token.as_str()),
            typed_name: "prod",
            allow_overwrite: false,
        };

        let first = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                submission(),
                |_| Vec::new(),
            )
            .await;
        assert!(first.is_ok(), "첫 제출은 통과해야 함");

        let second = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                submission(),
                |_| Vec::new(),
            )
            .await;
        assert!(
            matches!(
                second.err(),
                Some(ConfirmError::Rejected(GuardRejection::InvalidToken))
            ),
            "같은 토큰의 재사용(재생)은 거부되어야 함"
        );
    }

    /// TTL이 지난 토큰은 거부된다. 이 테스트는 `confirm_and_gate` 대신 그 내부의
    /// `check_and_consume`을 직접 부른다 — 실제 시계를 흔들지 않고(`issue_for_test` doc
    /// 참조) 발급 시각에서 **덧셈으로만** 미래 `now`를 만들어 넘긴다.
    #[tokio::test]
    async fn expired_token_is_rejected() {
        let guard = DestructiveGuard::new();
        let target = profile("prod");
        let context = sample_context(&target);

        let (token, issued_at) = guard.issue_for_test("restore.run", "prod").await;
        let token_str = token.to_string();
        let far_future = issued_at + CONFIRM_TTL + Duration::from_secs(1);

        let rejection = guard
            .check_and_consume(
                &context,
                &ConfirmSubmission {
                    token: Some(&token_str),
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                far_future,
            )
            .await;
        assert_eq!(rejection, Some(GuardRejection::InvalidToken));
    }

    /// 토큰이 다른 action/target에 발급된 것이면(예: prune 화면의 토큰을 restore 실행에
    /// 제출) 거부된다.
    #[tokio::test]
    async fn token_bound_to_a_different_action_or_target_is_rejected() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let mut prune_request = sample_request(&target);
        prune_request.what.action = "prune.run";
        let markup = guard
            .render_confirm(Lang::En, prune_request)
            .await
            .into_string();
        let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

        // 같은 프로파일이지만 restore.run으로 제출 — action이 다르다.
        let result = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                ConfirmSubmission {
                    token: Some(&token),
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;
        assert!(matches!(
            result.err(),
            Some(ConfirmError::Rejected(
                GuardRejection::ActionOrTargetMismatch
            ))
        ));

        // 다른 프로파일 대상 토큰도 같은 이유로 거부된다.
        let other_target = profile("staging");
        let markup2 = guard
            .render_confirm(Lang::En, sample_request(&other_target))
            .await
            .into_string();
        let token2 = extract_hidden_value(&markup2, FIELD_CONFIRM_TOKEN);
        let result2 = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target), // target은 "prod"인데 토큰은 "staging" 것
                ConfirmSubmission {
                    token: Some(&token2),
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;
        assert!(matches!(
            result2.err(),
            Some(ConfirmError::Rejected(
                GuardRejection::ActionOrTargetMismatch
            ))
        ));
    }

    /// 타이핑한 이름이 다르면(HTML 태그·제어문자·매우 긴 문자열 포함) 거부되고 패닉하지
    /// 않는다.
    #[tokio::test]
    async fn hostile_typed_names_are_rejected_without_panicking() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let huge = "a".repeat(1_000_000);
        let hostile_inputs = [
            "prod2",
            "<script>alert(1)</script>",
            "prod\0\x01\x02",
            huge.as_str(),
            "",
        ];

        for typed_name in hostile_inputs {
            let markup = guard
                .render_confirm(Lang::En, sample_request(&target))
                .await
                .into_string();
            let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

            let result = guard
                .confirm_and_gate(
                    &audit,
                    &same_origin_headers(),
                    sample_context(&target),
                    ConfirmSubmission {
                        token: Some(&token),
                        typed_name,
                        allow_overwrite: false,
                    },
                    |_| Vec::new(),
                )
                .await;
            assert!(
                matches!(
                    result.err(),
                    Some(ConfirmError::Rejected(GuardRejection::NameMismatch))
                ),
                "입력 {typed_name:?}은 거부되어야 함"
            );
        }
    }

    /// 거부된 확인은 감사 로그에 `.reject` 접미 action으로 남는다(모듈 헤더 설계 결정 4) —
    /// 실제 타이핑 원문은 로그에 실리지 않는다.
    #[tokio::test]
    async fn rejected_confirmation_is_logged_with_reject_suffix_and_no_raw_input() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let markup = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await
            .into_string();
        let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

        let hostile_name = "<script>alert(document.cookie)</script>";
        let result = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                ConfirmSubmission {
                    token: Some(&token),
                    typed_name: hostile_name,
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;
        assert!(result.is_err());

        let lines = read_lines(audit.path());
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0]["action"], "restore.run.reject");
        assert_eq!(lines[0]["target"], "prod");
        assert_eq!(lines[0]["outcome"], "failure");
        assert_eq!(
            lines[0]["args_masked"],
            serde_json::json!(["reason=name_mismatch"])
        );
        let raw = std::fs::read_to_string(audit.path()).unwrap();
        assert!(
            !raw.contains("script"),
            "타이핑한 원문이 감사 로그에 그대로 남았다: {raw}"
        );
    }

    /// 출처(Origin) 불일치는 감사에 남기지 않는다 — CSRF로 튕겨난 요청에 "누군가
    /// 시도했다"는 기록을 붙이지 않는다는 판단(모듈 헤더 설계 결정 4).
    #[tokio::test]
    async fn foreign_origin_is_rejected_and_not_logged() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let mut headers = HeaderMap::new();
        headers.insert("host", HeaderValue::from_static("console.example"));
        headers.insert(
            axum::http::header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );

        let result = guard
            .confirm_and_gate(
                &audit,
                &headers,
                sample_context(&target),
                ConfirmSubmission {
                    token: None,
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;
        assert!(matches!(result.err(), Some(ConfirmError::ForeignOrigin(_))));
        assert!(
            read_lines(audit.path()).is_empty(),
            "출처 불일치는 기록되면 안 됨"
        );
    }

    /// 감사 로그 쓰기가 실패하면(읽기 전용 파일) `ConfirmError::Audit`로 나오고, 거부
    /// (`Rejected`)와는 다른 갈래다 — 이름·토큰은 맞았다는 뜻이기 때문이다.
    #[cfg(unix)]
    #[tokio::test]
    async fn audit_write_failure_surfaces_as_audit_variant() {
        use std::os::unix::fs::PermissionsExt;

        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        let markup = guard
            .render_confirm(Lang::En, sample_request(&target))
            .await
            .into_string();
        let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

        std::fs::set_permissions(audit.path(), std::fs::Permissions::from_mode(0o400)).unwrap();
        let result = guard
            .confirm_and_gate(
                &audit,
                &same_origin_headers(),
                sample_context(&target),
                ConfirmSubmission {
                    token: Some(&token),
                    typed_name: "prod",
                    allow_overwrite: false,
                },
                |_| Vec::new(),
            )
            .await;
        std::fs::set_permissions(audit.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        assert!(matches!(result.err(), Some(ConfirmError::Audit(_))));
    }

    /// 덮어쓰기 토글은 입력을 충실히 반영한다 — 꺼짐이 기본이고, 그 값은 호출부가 argv를
    /// 조립할 때 반드시 참조해야 한다(모듈 헤더 설계 결정 5).
    #[tokio::test]
    async fn allow_overwrite_is_echoed_faithfully() {
        let guard = DestructiveGuard::new();
        let dir = tempfile::tempdir().unwrap();
        let audit = AuditLog::open(dir.path()).unwrap();
        let target = profile("prod");

        for expected in [false, true] {
            let markup = guard
                .render_confirm(Lang::En, sample_request(&target))
                .await
                .into_string();
            let token = extract_hidden_value(&markup, FIELD_CONFIRM_TOKEN);

            let confirmed = guard
                .confirm_and_gate(
                    &audit,
                    &same_origin_headers(),
                    sample_context(&target),
                    ConfirmSubmission {
                        token: Some(&token),
                        typed_name: "prod",
                        allow_overwrite: expected,
                    },
                    |_| Vec::new(),
                )
                .await
                .unwrap();
            assert_eq!(confirmed.allow_overwrite, expected);
        }
    }

    /// 만료된 항목은 청소된다(메모리가 무한히 늘지 않는다) — `sweep_expired_at`에 미래
    /// `now`를 직접 넘겨 검증한다(`expired_token_is_rejected`와 같은 이유로 실제 시계는
    /// 흔들지 않는다).
    #[tokio::test]
    async fn expired_entries_are_swept() {
        let guard = DestructiveGuard::new();

        let (_token, issued_at) = guard.issue_for_test("restore.run", "prod").await;
        assert_eq!(guard.pending_len().await, 1);

        // 아직 안 지났다 — 청소되지 않는다.
        guard.sweep_expired_at(issued_at + CONFIRM_TTL).await;
        assert_eq!(guard.pending_len().await, 1, "TTL 안쪽은 청소되면 안 됨");

        // TTL을 넘겼다 — 청소된다.
        guard
            .sweep_expired_at(issued_at + CONFIRM_TTL + Duration::from_secs(1))
            .await;
        assert_eq!(
            guard.pending_len().await,
            0,
            "TTL을 넘긴 항목은 청소돼야 함"
        );
    }

    /// [`is_expired`]의 경계값 — TTL과 정확히 같으면 아직 만료가 아니고, 1을 넘으면
    /// 만료다(`crate::web::auth`의 `decay_failures_removes_one_step_per_period`와 같은
    /// 순수 함수 경계 테스트).
    #[tokio::test]
    async fn is_expired_boundary_is_correct() {
        let t0 = Instant::now();
        assert!(!is_expired(t0, t0), "발급 직후는 만료가 아니다");
        assert!(
            !is_expired(t0, t0 + CONFIRM_TTL),
            "TTL과 정확히 같은 시각은 아직 만료가 아니다(경계 포함)"
        );
        assert!(
            is_expired(t0, t0 + CONFIRM_TTL + Duration::from_secs(1)),
            "TTL을 넘기면 만료다"
        );
    }

    /// 렌더된 마크업에서 hidden 필드 값을 뽑는다 — 테스트 전용 최소 파서.
    fn extract_hidden_value(markup: &str, field: &str) -> String {
        let marker = format!(r#"name="{field}" value=""#);
        let start = markup
            .find(&marker)
            .unwrap_or_else(|| panic!("필드 '{field}'를 찾지 못했다: {markup}"))
            + marker.len();
        let end = markup[start..].find('"').unwrap() + start;
        markup[start..end].to_string()
    }
}
