//! [`JobOutcome`](super::runner::JobOutcome)을 운영자용 화면 문구로 접는 계층.
//!
//! ## 이 파일이 하는 일과 하지 않는 일
//! t10의 `job/runner.rs`가 이미 자식 종료 코드 0~5·시그널·미상을 [`JobOutcome`]의 아홉
//! variant로 분류해 놨다(그 파일 헤더 "종료 코드 0~5를 뭉개지 않는다" 참조). 이 파일은 그
//! 분류를 **다시 하지 않는다** — `from_status`도, exit code ↔ variant 매핑도 여기 없다.
//! 하는 일은 딱 하나, 이미 분류된 [`JobOutcome`] 하나를 받아 화면에 낼
//! [`Level`](components::Level)·헤드라인·안내문 3종 세트로 바꾸는 것뿐이다([`present`]).
//!
//! ## variant가 아홉 개인데 인계 지시서는 "여덟 개"라고 했다
//! 지시서는 "exit code 0~5(여섯) + 시그널(하나) + 미상(하나) = 여덟 상태"라고 셌지만, 실제
//! [`JobOutcome`]에는 [`JobOutcome::UnexpectedExit`](PRD 밖의 종료 코드, 6~255)이 하나 더
//! 있다 — 아홉 번째다. 이 파일은 실제 타입 정의를 기준으로 **아홉 variant 전부**를
//! 다루고, 아래 테스트가 아홉 개를 모두 순회해 하나도 빠뜨리지 않았음을 고정한다.
//!
//! ## 왜 순수 함수인가
//! [`present`]는 자식 프로세스도, 파일시스템도, 시각(now)도 건드리지 않는다 — 입력은
//! `&JobOutcome`과 `Lang` 둘뿐이고 출력은 값이다. 그래서 아홉 상태 × 2개 언어 = 열여덟
//! 케이스를 자식을 한 번도 띄우지 않고 단위 테스트로 전부 고정할 수 있다
//! ([`crate::web::routes`] 모듈 헤더의 "판정·렌더는 순수 함수" 규약과 같은 이유).
//!
//! ## [`Level`]이 네 칸뿐인데 상태는 아홉 개다 — 레벨은 겹쳐도 문구는 겹치지 않는다
//! [`components::Level`]은 화면 전체가 공유하는 유일한 상태 축이라 `Ok`/`Warn`/`Fail`/
//! `Error` 네 값뿐이다(그 모듈 헤더 "이 화면이 있는 이유" 참조). 아홉 상태를 네 레벨에
//! 욱여넣으면 필연적으로 여러 상태가 같은 레벨을 공유한다 — 예를 들어
//! [`JobOutcome::SucceededWithWarnings`](exit 4)와 [`JobOutcome::LockConflict`](exit 5)는
//! 둘 다 [`Level::Warn`]이다("실패가 아니다"라는 공통점 때문). 레벨이 같아도 **헤드라인은
//! 절대 겹치지 않게** 만든다 — 배지 색이 같아 보여도 글자를 읽으면 "경고 동반 성공"과
//! "다른 인스턴스가 실행 중"이 다른 이야기라는 것을 바로 알 수 있어야 한다.
//!
//! ## 레벨 매핑은 이 파일에만 있다 — [`level_for_label`]
//! 한동안 `view/jobs.rs`가 자기 사본([`Level`]로 옮기는 `match`)을 들고 있었고, 실제로
//! **두 곳이 갈라졌다**: exit 2(`rejected`)와 `unexpected-exit`가 이 파일에서는
//! [`Level::Error`], 그쪽에서는 [`Level::Fail`]이었다. 같은 잡이 화면마다 다른 색으로
//! 보이는 상태였다는 뜻이다. 그래서 매핑을 [`level_for_label`] 하나로 모으고, [`present`]
//! 자신도 자기 `match` arm에 레벨을 적어 넣지 않고 그 함수를 부른다 — **사본이 하나도
//! 없으면 갈라질 수 없다.**
//!
//! 라벨(문자열)을 키로 쓰는 것이 핵심이다. 레벨은 payload에 의존하지 않으므로
//! (`Signaled(9)`와 `Signaled(15)`가 다른 색이어야 할 이유가 없다) 라벨만으로 정할 수 있고,
//! 그래서 결과를 **문자열로 영속하는** 저장소([`crate::web::state::jobs`])도 역변환 없이
//! 같은 함수를 그대로 쓸 수 있다. `label → JobOutcome` 역변환을 만들었다면
//! `Signaled(i32)`의 시그널 번호가 라벨에서 소실되어(저장소의 `exit_code`는 시그널 종료 시
//! `None`이다) 재구성이 불가능했을 것이다 — 레벨만 라벨로 가르면 그 문제를 아예 만나지
//! 않는다.
//!
//! ## 레벨 배정 근거(doctor.rs의 선례를 그대로 따른다)
//! - [`JobOutcome::Rejected`](exit 2, 웹이 잘못된 인자를 보냄) → [`Level::Error`].
//!   [`crate::web::routes::doctor::Verdict::Misconfigured`](exit 2)와 같은 논리다 — "점검
//!   자체가 성립하지 않았다"에 해당하는 게 "작업이 우리 잘못으로 시작조차 못 했다"다.
//! - [`JobOutcome::PrecheckFailed`](exit 3, 대상 서버 문제) → [`Level::Fail`].
//!   `Verdict::Blocked`(exit 3)와 같은 논리다 — "점검은 됐고, 대상이 막혔다"에 해당하는
//!   게 "대상 서버·설정을 고쳐야 한다"다.
//!   **exit 2와 3을 반대로 두지 않는다** — 2는 우리(콘솔) 책임, 3은 운영자가 대상을 고칠
//!   책임이고, `Error`/`Fail`의 의미(점검 불가 vs 점검됨-차단)가 정확히 그 구분과 맞아
//!   떨어진다.
//! - [`JobOutcome::LockConflict`](exit 5) → [`Level::Warn`], **`retryable = true`**.
//!   "실패가 아니라 나중에 그대로 다시 하면 되는 상태"이므로 Fail이 아니라 Warn이다.
//!   안내문에 [`crate::web::routes::lock::LOCK_PATH`]를 실어 "누가 잡고 있는지"로
//!   연결한다(t13 지시서 요구사항) — 운영자가 뜬금없이 재시도를 반복하지 않고 그 화면에서
//!   원인(다른 인스턴스)을 먼저 본다.
//! - [`JobOutcome::UnexpectedExit`]·[`JobOutcome::Signaled`]·[`JobOutcome::Unknown`] →
//!   [`Level::Error`]. 셋 다 "무슨 일이 있었는지 이 프로세스가 확신할 수 없다"는 공통점이
//!   있다(전자 둘은 우리 어휘 밖의 종료 방식이고, 후자는 정보 자체가 없다). `doctor.rs`의
//!   `Verdict::Unexpected` → `Level::Error`와 같은 판단이다.
//!   **`unexpected-exit`를 [`Level::Fail`]로 두지 않는 이유**가 여기 있다: `Fail`은
//!   "점검은 됐고 대상이 막혔다 = 운영자가 대상을 고칠 일"인데, PRD 밖 종료 코드는 운영자가
//!   대상 서버를 아무리 뒤져도 원인이 없다(콘솔과 잡 바이너리가 다른 빌드이거나 자식이
//!   우리 에러 모델을 거치지 않고 죽은 것이다). 고칠 주체가 다르면 레벨도 달라야 한다.

use crate::i18n::Lang;
use crate::web::job::runner::JobOutcome;
use crate::web::view::components::Level;

/// [`present`]가 돌려주는, 화면이 그대로 쓸 수 있는 최소 표현.
///
/// 필드 셋을 `JobOutcome` 자체(t10)나 [`Level`](components::Level)에 더 넣지 않고 여기
/// 별도 구조체로 둔 이유: `JobOutcome`은 "무엇이 일어났는가"(사실)를 표현하고, 이 구조체는
/// "그것을 사람에게 어떻게 보여줄 것인가"(표현)를 표현한다. 하나로 합치면 t10의 파일이
/// i18n 문구까지 알아야 하고, 문구를 고치는 태스크가 종료 코드 분류 코드를 건드리게 된다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitPresentation {
    /// 배지·색을 정하는 의미 레벨. 화면은 이 값만으로 `data-level`을 채운다.
    pub level: Level,
    /// 한 줄 요약 — 스캔하는 사람이 배지 옆에서 읽는다.
    pub headline: String,
    /// 근거(종료 코드)와 다음 행동. 시그널·미상처럼 "종료 코드"라는 개념이 없는 상태에도
    /// 항상 채운다 — 운영자가 다음에 뭘 해야 하는지는 어떤 상태에서도 빠지면 안 된다.
    pub detail: String,
    /// 그대로 같은 요청을 다시 보내면 되는 상태인지. [`JobOutcome::retryable`]과 같은
    /// 값이지만, 화면이 `JobOutcome`을 몰라도(이 구조체만 받아도) 재시도 버튼을 보여줄지
    /// 판단할 수 있도록 여기 복제해 둔다.
    pub retryable: bool,
}

/// [`JobOutcome::label`] 어휘 하나를 화면 레벨로 옮긴다 — **결과→레벨 매핑의 단일 출처**
/// (모듈 헤더 "레벨 매핑은 이 파일에만 있다" 참조).
///
/// 화면 세 곳이 이 함수를 공유한다: 잡 이력 목록·상세([`crate::web::view::jobs`]), 백업
/// 라이브 화면([`crate::web::view::backup`]), 그리고 [`present`] 자신. 저장소는 결과를
/// 문자열로 남기므로([`crate::web::state::jobs::JobSummary::outcome`]) 화면들은 라벨만 들고
/// 있고, 이 함수는 그 라벨을 그대로 받는다.
///
/// ## 모르는 라벨은 [`Level::Error`]다
/// 나중 빌드가 새 결과를 추가했을 때 그것이 조용히 초록([`Level::Ok`])으로 칠해지면 화면이
/// 거짓말을 한다(`routes::doctor::level_from_status`와 같은 판단). 대문자·오타·빈 문자열도
/// 같은 취급이다 — 어휘는 [`JobOutcome::label`]이 내놓는 것 그대로만 유효하다.
pub fn level_for_label(label: &str) -> Level {
    match label {
        // exit 0 — 문제 없이 끝났다.
        "succeeded" => Level::Ok,
        // exit 4 — 경고를 동반한 **성공**. 실패로 칠하면 운영자가 멀쩡한 백업을 다시 돌린다.
        "succeeded-with-warnings" => Level::Warn,
        // exit 5 — 아무 일도 일어나지 않았고 그대로 재시도하면 된다. 실패가 아니다.
        "lock-conflict" => Level::Warn,
        // exit 1 — 시작해서 돌다가 실패했다. 로그를 보고 원인을 고칠 대상이다.
        "failed" => Level::Fail,
        // exit 3 — 점검은 됐고 대상이 막혔다. 운영자가 대상 서버·설정을 고친다.
        "precheck-failed" => Level::Fail,
        // exit 2 — 웹이 만든 argv가 CLI 사용법을 위반했다. **우리 잘못**이라 운영자가 config를
        // 뒤져도 원인이 없다 — 대상 서버 문제(Fail)와 구분되어야 한다(모듈 헤더 참조).
        "rejected" => Level::Error,
        // 우리 에러 모델을 거치지 않고 죽었다(PRD 밖 코드) — 예상 밖이므로 Fail이 아니다.
        "unexpected-exit" => Level::Error,
        // 잡 자신이 판정한 결과가 아니다(밖에서 끊겼거나 결과를 얻지 못했다).
        "signaled" | "unknown" => Level::Error,
        _ => Level::Error,
    }
}

/// 잡 종료 상태를 화면 표현으로 접는다 — **순수 함수**.
///
/// 아홉 variant(모듈 헤더 참조)를 전부 다르게 처리한다. `Lang`은 설명 문장(`detail`)에만
/// 적용되고, `headline`도 설명 문장이므로 언어를 탄다 — 다만 기술 용어(`exit N`, `pid`,
/// 경로)는 [`crate::i18n`] 규약대로 어느 언어에서도 영문 그대로 남는다.
///
/// `level`은 arm마다 적어 넣지 않고 [`level_for_label`] 한 번으로 정한다 — 그래야 화면들과
/// 같은 매핑을 쓴다는 것이 코드 구조로 보장된다(모듈 헤더 참조).
pub fn present(outcome: &JobOutcome, lang: Lang) -> ExitPresentation {
    let level = level_for_label(outcome.label());
    match outcome {
        JobOutcome::Succeeded => ExitPresentation {
            level,
            headline: lang
                .sel("Job succeeded.", "작업이 성공했습니다.")
                .to_string(),
            detail: lang
                .sel(
                    "exit 0 — completed cleanly. Nothing to review.",
                    "exit 0 — 문제 없이 끝났습니다. 확인할 것이 없습니다.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::SucceededWithWarnings => ExitPresentation {
            level,
            headline: lang
                .sel(
                    "Job succeeded, with warnings.",
                    "작업이 성공했습니다(경고 있음).",
                )
                .to_string(),
            detail: lang
                .sel(
                    "exit 4 — this is success with caveats, not a failure. The job finished \
                     and produced output. Review the warnings below; nothing here needs a \
                     retry.",
                    "exit 4 — 이것은 실패가 아니라 '단서가 붙은 성공'입니다. 작업은 끝났고 \
                     산출물이 있습니다. 아래 경고를 확인하세요 — 다시 돌릴 필요는 없습니다.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::Failed => ExitPresentation {
            level,
            headline: lang.sel("Job failed.", "작업이 실패했습니다.").to_string(),
            detail: lang
                .sel(
                    "exit 1 — the job started and ran, but did not finish successfully. Read \
                     the log for the cause, fix it, then run it again.",
                    "exit 1 — 작업이 시작되어 실행됐지만 성공적으로 끝나지 못했습니다. \
                     로그에서 원인을 확인하고 고친 뒤 다시 실행하세요.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::Rejected => ExitPresentation {
            level,
            headline: lang
                .sel(
                    "The console sent a request the job could not accept.",
                    "콘솔이 작업이 받아들일 수 없는 요청을 보냈습니다.",
                )
                .to_string(),
            detail: lang
                .sel(
                    "exit 2 — usage/configuration error. The job never started, so nothing \
                     changed. This is the console's fault, not the target server's — retrying \
                     the exact same request will fail again the same way. Please report it.",
                    "exit 2 — 사용법·설정 오류입니다. 작업은 시작되지 않았고 아무것도 \
                     바뀌지 않았습니다. 이건 대상 서버가 아니라 콘솔 쪽 잘못이라, 같은 요청을 \
                     그대로 다시 보내도 똑같이 실패합니다. 문제를 보고해 주세요.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::PrecheckFailed => ExitPresentation {
            level,
            headline: lang
                .sel(
                    "Pre-flight check on the target failed.",
                    "대상 서버 사전 점검이 실패했습니다.",
                )
                .to_string(),
            detail: lang
                .sel(
                    "exit 3 — the job never started, so nothing changed. Fix the target server \
                     or its configuration (not the console) before trying again — retrying \
                     without a fix will fail the same way.",
                    "exit 3 — 작업은 시작되지 않았고 아무것도 바뀌지 않았습니다. 콘솔이 \
                     아니라 대상 서버·설정을 먼저 고쳐야 합니다 — 고치지 않고 재시도하면 \
                     같은 결과가 납니다.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::LockConflict => {
            let base = lang.sel(
                "exit 5 — this is not a failure, and nothing changed. The same profile is \
                 locked by another instance (it may be a cron-run CLI, not another web \
                 request). Wait for it to finish, then retry the exact same request.",
                "exit 5 — 이것은 실패가 아니며 아무것도 바뀌지 않았습니다. 같은 프로파일을 \
                 다른 인스턴스(웹이 아니라 cron이 띄운 CLI일 수도 있습니다)가 잡고 있습니다. \
                 끝나기를 기다렸다가 같은 요청을 그대로 다시 보내세요.",
            );
            let pointer = lang.sel("Who is holding it:", "누가 잡고 있는지:");
            ExitPresentation {
                level,
                headline: lang
                    .sel(
                        "Another run is already in progress.",
                        "다른 작업이 이미 실행 중입니다.",
                    )
                    .to_string(),
                detail: format!("{base} {pointer} {}", crate::web::routes::lock::LOCK_PATH),
                retryable: true,
            }
        }

        JobOutcome::UnexpectedExit(code) => ExitPresentation {
            level,
            headline: format!(
                "{} (exit {code})",
                lang.sel(
                    "Job exited with a code this console does not know.",
                    "작업이 이 콘솔이 모르는 종료 코드로 끝났습니다."
                )
            ),
            detail: lang
                .sel(
                    "This code is outside the 0-5 range the console understands. The console \
                     and the job binary may be different builds — check versions before \
                     assuming anything about what happened to the data.",
                    "이 코드는 콘솔이 아는 0~5 범위 밖입니다. 콘솔과 작업 바이너리가 다른 \
                     빌드일 수 있습니다 — 데이터에 무슨 일이 있었는지 단정하기 전에 버전부터 \
                     확인하세요.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::Signaled(signal) => ExitPresentation {
            level,
            headline: format!(
                "{} (signal {signal})",
                lang.sel(
                    "Job was terminated from outside.",
                    "작업이 밖에서 종료되었습니다."
                )
            ),
            detail: lang
                .sel(
                    "The job did not decide its own outcome — it was killed (cancellation, an \
                     out-of-memory kill, or a crash). Output may be partially written. Check \
                     the destination before trusting it, and do not assume the same result if \
                     you retry.",
                    "작업이 스스로 결과를 정하지 못했습니다 — 밖에서 종료됐습니다(취소, OOM \
                     kill, 크래시 등). 산출물이 반쯤 남아 있을 수 있습니다. 신뢰하기 전에 \
                     목적지를 확인하고, 다시 돌린다고 같은 결과가 나온다고 가정하지 마세요.",
                )
                .to_string(),
            retryable: false,
        },

        JobOutcome::Unknown => ExitPresentation {
            level,
            headline: lang
                .sel(
                    "Job outcome could not be determined.",
                    "작업 결과를 판정할 수 없습니다.",
                )
                .to_string(),
            detail: lang
                .sel(
                    "Neither an exit code nor a signal was available. This should not happen \
                     on a supported platform — treat this as unknown, not as success.",
                    "종료 코드도 시그널 번호도 얻지 못했습니다. 지원 플랫폼에서는 일어나지 \
                     않아야 하는 상황입니다 — 성공이 아니라 '판정 불가'로 취급하세요.",
                )
                .to_string(),
            retryable: false,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 이 테스트가 도는 아홉 variant의 대표값 — 모듈 헤더 "아홉 개인데 지시서는 여덟"의
    /// 실측이다. `UnexpectedExit`/`Signaled`는 대표 코드 하나씩만 쓴다(값 자체가 아니라
    /// variant 종류가 서로 다른 문구를 내는지가 관심사이므로).
    fn all_outcomes() -> [JobOutcome; 9] {
        [
            JobOutcome::Succeeded,
            JobOutcome::Failed,
            JobOutcome::Rejected,
            JobOutcome::PrecheckFailed,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::LockConflict,
            JobOutcome::UnexpectedExit(42),
            JobOutcome::Signaled(15),
            JobOutcome::Unknown,
        ]
    }

    /// 아홉 상태 전부가 ko·en 양쪽에서 서로 다른 헤드라인을 낸다 — 레벨이 겹치더라도
    /// 글자로는 절대 겹치지 않는다는 성질을 고정한다(모듈 헤더 참조).
    #[test]
    fn all_nine_outcomes_have_distinct_headlines_in_both_languages() {
        for lang in [Lang::En, Lang::Ko] {
            let outcomes = all_outcomes();
            let mut headlines: Vec<String> =
                outcomes.iter().map(|o| present(o, lang).headline).collect();
            let total = headlines.len();
            headlines.sort();
            headlines.dedup();
            assert_eq!(
                headlines.len(),
                total,
                "{lang:?}에서 헤드라인이 겹치는 상태가 있다"
            );
        }
    }

    /// **exit 4는 `Fail` 레벨로 매핑되지 않는다** — 경고 동반 성공이다.
    #[test]
    fn exit_four_is_never_fail_level() {
        let p = present(&JobOutcome::SucceededWithWarnings, Lang::En);
        assert_ne!(p.level, Level::Fail);
        assert_eq!(p.level, Level::Warn);
        assert!(!p.retryable, "이미 성공했으므로 재시도 대상이 아니다");
        for lang in [Lang::En, Lang::Ko] {
            let p = present(&JobOutcome::SucceededWithWarnings, lang);
            assert!(
                p.headline.to_lowercase().contains("succeeded") || p.headline.contains("성공"),
                "{lang:?} 헤드라인이 성공임을 말하지 않는다: {}",
                p.headline
            );
        }
    }

    /// **exit 5는 `Fail` 레벨로 매핑되지 않고, 재시도 가능으로 표시된다.**
    #[test]
    fn exit_five_is_never_fail_and_is_retryable() {
        let p = present(&JobOutcome::LockConflict, Lang::En);
        assert_ne!(p.level, Level::Fail);
        assert!(p.retryable, "락 충돌은 재시도 가능해야 한다");
        // 다른 유일한 재시도 가능 상태가 없어야 한다 — retryable=true가 락 충돌만의 신호다.
        for outcome in all_outcomes() {
            if outcome == JobOutcome::LockConflict {
                continue;
            }
            assert!(
                !present(&outcome, Lang::En).retryable,
                "{outcome:?}가 락 충돌이 아닌데 재시도 가능으로 표시됐다"
            );
        }
    }

    /// 락 충돌 안내문이 "누가 잡고 있는지" 확인할 화면(`/lock`) 경로를 함께 준다.
    #[test]
    fn lock_conflict_points_to_the_lock_screen() {
        for lang in [Lang::En, Lang::Ko] {
            let p = present(&JobOutcome::LockConflict, lang);
            assert!(
                p.detail.contains(crate::web::routes::lock::LOCK_PATH),
                "{lang:?} 안내문에 락 화면 경로가 없다: {}",
                p.detail
            );
        }
    }

    /// exit 2(우리 잘못)와 exit 3(대상 서버 문제)은 레벨도, 안내문도 다르다 — 운영자가
    /// 취해야 할 행동이 다르기 때문이다.
    #[test]
    fn rejected_and_precheck_failed_give_different_guidance() {
        let rejected = present(&JobOutcome::Rejected, Lang::En);
        let precheck = present(&JobOutcome::PrecheckFailed, Lang::En);
        assert_ne!(rejected.level, precheck.level);
        assert_ne!(rejected.headline, precheck.headline);
        assert_ne!(rejected.detail, precheck.detail);
        // exit 2는 "콘솔 잘못", exit 3은 "대상 서버"를 명시한다 — 문구가 책임 소재를 말한다.
        assert!(rejected.detail.contains("console"));
        assert!(precheck.detail.contains("target"));

        let rejected_ko = present(&JobOutcome::Rejected, Lang::Ko);
        let precheck_ko = present(&JobOutcome::PrecheckFailed, Lang::Ko);
        assert!(rejected_ko.detail.contains("콘솔"));
        assert!(precheck_ko.detail.contains("대상"));
    }

    /// 시작 여부가 다른 세 그룹(시작 안 함/시작해서 실패/시작해서 성공)이 레벨에서도
    /// 뒤섞이지 않는다 — `JobOutcome::started()`가 참인 것과 거짓인 것이 최소한 서로 다른
    /// 조합으로 나온다는 것을 간접 확인한다.
    #[test]
    fn not_started_outcomes_are_never_ok_level() {
        for outcome in [
            JobOutcome::Rejected,
            JobOutcome::PrecheckFailed,
            JobOutcome::LockConflict,
        ] {
            assert!(!outcome.started(), "테스트 전제가 깨졌다: {outcome:?}");
            assert_ne!(
                present(&outcome, Lang::En).level,
                Level::Ok,
                "시작조차 안 한 상태가 Ok로 보이면 안 된다: {outcome:?}"
            );
        }
    }

    /// 시그널·미상·PRD 밖 종료 코드는 전부 `Error` 레벨이다 — "무슨 일이 있었는지 이
    /// 프로세스가 확신할 수 없다"는 공통점을 공유한다(모듈 헤더 레벨 배정 근거).
    #[test]
    fn unknown_shaped_outcomes_are_error_level() {
        for outcome in [
            JobOutcome::UnexpectedExit(99),
            JobOutcome::Signaled(9),
            JobOutcome::Unknown,
        ] {
            assert_eq!(present(&outcome, Lang::En).level, Level::Error);
        }
    }

    /// 시그널 종료 안내문은 "산출물이 반쯤 남아 있을 수 있다"를 명시한다 — 시그널은 잡
    /// 자신의 판정이 아니라 외부에서 끊긴 것이므로 실패와 다르게 취급해야 한다.
    #[test]
    fn signaled_detail_warns_about_partial_output() {
        let en = present(&JobOutcome::Signaled(9), Lang::En);
        assert!(en.detail.contains("partial") || en.detail.to_lowercase().contains("killed"));
        let ko = present(&JobOutcome::Signaled(9), Lang::Ko);
        assert!(ko.detail.contains("반쯤") || ko.detail.contains("종료"));
    }

    /// **[`present`]의 레벨은 항상 [`level_for_label`]에서 나온다.**
    ///
    /// 이 테스트가 이 파일에서 가장 중요한 회귀 방어다 — 누가 `present`의 arm에 레벨
    /// 리터럴을 다시 적어 넣으면(예전에 두 곳이 갈라졌던 그 방식으로) 여기서 걸린다.
    /// payload가 있는 두 variant는 값을 바꿔가며 확인한다: 레벨이 payload에 의존하기
    /// 시작하면 "라벨만으로 레벨을 정할 수 있다"는 전제(모듈 헤더)가 깨진다.
    #[test]
    fn present_level_always_comes_from_level_for_label() {
        for lang in [Lang::En, Lang::Ko] {
            for outcome in all_outcomes() {
                assert_eq!(
                    present(&outcome, lang).level,
                    level_for_label(outcome.label()),
                    "{outcome:?}의 레벨이 level_for_label과 다르다"
                );
            }
            for code in [6, 42, 127, 255] {
                let outcome = JobOutcome::UnexpectedExit(code);
                assert_eq!(
                    present(&outcome, lang).level,
                    level_for_label(outcome.label())
                );
            }
            for signal in [2, 9, 15] {
                let outcome = JobOutcome::Signaled(signal);
                assert_eq!(
                    present(&outcome, lang).level,
                    level_for_label(outcome.label())
                );
            }
        }
    }

    /// exit 2(`rejected`)와 `unexpected-exit`는 [`Level::Error`]다 — [`Level::Fail`]이
    /// 아니다.
    ///
    /// 둘은 **우리 쪽 문제**(웹이 만든 argv가 CLI 사용법을 위반했다 / 자식이 우리 에러
    /// 모델을 거치지 않고 죽었다)이므로, 운영자가 대상 서버·config를 뒤져도 원인이 없다.
    /// `Fail`("점검은 됐고 대상이 막혔다")로 칠하면 운영자를 엉뚱한 곳으로 보낸다.
    #[test]
    fn our_fault_outcomes_are_error_not_fail() {
        for label in ["rejected", "unexpected-exit"] {
            assert_eq!(level_for_label(label), Level::Error, "{label}");
            assert_ne!(level_for_label(label), Level::Fail, "{label}");
        }
        // 대상 서버 쪽 문제는 그대로 Fail이다 — 둘이 뭉개지지 않았음을 함께 고정한다.
        assert_eq!(level_for_label("precheck-failed"), Level::Fail);
        assert_eq!(level_for_label("failed"), Level::Fail);
    }

    /// 아홉 라벨이 전부 아는 어휘로 처리되고, 모르는 문자열만 기본값 [`Level::Error`]로
    /// 떨어진다. 성공 계열이 실패·미상으로 칠해지지 않는지도 함께 본다.
    #[test]
    fn level_for_label_covers_the_whole_vocabulary() {
        for outcome in all_outcomes() {
            let level = level_for_label(outcome.label());
            if outcome.is_success() {
                assert_ne!(level, Level::Fail, "{outcome:?}는 성공인데 실패로 칠했다");
                assert_ne!(
                    level,
                    Level::Error,
                    "{outcome:?}는 성공인데 미상으로 칠했다"
                );
            }
        }
        assert_eq!(level_for_label("succeeded"), Level::Ok);
        for unknown in ["", "SUCCEEDED", "future-outcome", "succeeded "] {
            assert_eq!(
                level_for_label(unknown),
                Level::Error,
                "'{unknown}'이 조용히 통과했다"
            );
        }
    }

    /// `detail`은 어떤 상태에서도 비어 있지 않다 — 운영자가 다음에 뭘 해야 하는지는
    /// 시그널·미상 같은 "코드가 없는" 상태에서도 반드시 있어야 한다.
    #[test]
    fn detail_is_never_empty() {
        for lang in [Lang::En, Lang::Ko] {
            for outcome in all_outcomes() {
                assert!(
                    !present(&outcome, lang).detail.is_empty(),
                    "{lang:?} {outcome:?}의 detail이 비어 있다"
                );
            }
        }
    }
}
