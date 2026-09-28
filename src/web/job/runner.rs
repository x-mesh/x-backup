//! 잡 러너 — `x-backup <cmd> --json` 자식 프로세스를 띄우고 종료 코드를 분류한다.
//!
//! 이 파일은 [`crate::web`] 모듈 헤더가 선언한 최상위 불변식("웹은 도메인 로직을
//! 재구현하지 않는다")이 **실제로 성립하게 만드는 계층**이다. 웹의 모든 명령 실행이 이
//! 파일을 지난다.
//!
//! ## 셸을 경유하지 않는다
//! [`tokio::process::Command`]에 프로그램과 인자를 각각 넘긴다 — `sh -c "..."` 형태는
//! 쓰지 않는다. 셸을 끼우면 인자 하나하나가 다시 파싱되면서 `;`·`$( )`·백틱·`&&`가
//! 명령 경계가 되고, "값 검증"이라는 방어가 문자열 조립 실수 한 번으로 무의미해진다.
//! 셸이 없으면 인자는 커널의 `execve` 인자 배열로 그대로 전달되며, 그 안의 어떤 문자도
//! 특별한 의미를 갖지 않는다. 아래 `child_does_not_go_through_a_shell` 테스트가 실제로
//! 자식을 띄워 이 성질을 확인한다.
//!
//! (참고: [`crate::hooks`]는 사용자가 config에 적은 셸 명령을 실행하는 것이 목적이므로
//! 의도적으로 `sh -c`를 쓴다. 그건 "운영자가 쓴 셸 스크립트를 돌린다"는 다른 기능이고,
//! 여기는 "웹 요청으로 우리 자신을 다시 돌린다"이므로 셸이 끼일 이유가 없다.)
//!
//! ## 자기 자신을 [`std::env::current_exe`]로 부른다
//! `Command::new("x-backup")`처럼 이름으로 부르지 않는다. 이름으로 부르면 `PATH`가
//! 결정권을 갖고, 그러면 세 가지가 무너진다:
//!
//! 1. **버전 드리프트.** 콘솔은 자기 자신이 아는 코드를 돌려야 한다. `PATH`에 다른
//!    x-backup(예: 예전 brew 설치본)이 있으면 화면과 실제 동작이 갈라진다.
//! 2. **PATH 하이재킹.** `PATH` 앞쪽 디렉터리에 쓰기 권한을 가진 누군가가 `x-backup`
//!    이라는 파일을 두면, 웹 요청 한 번이 그 파일을 실행한다.
//! 3. **테스트 가능성.** `current_exe()`는 결정적이라 "무엇을 실행하는가"를 단정할 수
//!    있다(아래 `new_resolves_program_from_current_exe`).
//!
//! **한계:** 리눅스에서 `current_exe()`는 `/proc/self/exe`를 따라가므로, `x-backup update`
//! 로 바이너리를 교체한 뒤에도 상주 서버는 **교체 전 inode**를 계속 실행한다. 이건 오히려
//! 원하는 동작이다 — 잡이 도는 중에 버전이 갈리지 않는다. 새 버전을 쓰려면 `serve`를
//! 재시작해야 하고, 이 사실을 운영 문서가 알려야 한다.
//!
//! ## 자식 env는 화이트리스트로 **새로 짓는다**
//! [`std::process::Command::env_clear`]로 서버 환경을 통째로 지우고, 필요한 것만 다시
//! 넣는다. "서버 환경을 물려주고 시크릿 몇 개를 지운다"(블랙리스트)는 방식은 쓰지 않는다 —
//! config에 새 `*_env` 항목이 하나 추가되거나 운영자가 새 시크릿을 export하는 순간
//! 조용히 샌다. 지워야 할 목록을 관리하는 실수는 유출이고, 넣어야 할 목록을 관리하는
//! 실수는 잡 실패다. 후자가 훨씬 나은 실패다.
//!
//! 화이트리스트는 [`FORWARDED_ENV_NAMES`]에 있고, **각 항목이 왜 필요한지**는 그
//! 상수의 doc에 하나씩 적어 두었다. 특히 `XDG_RUNTIME_DIR`을 빠뜨리면 파일 락이 조용히
//! 무력화된다(그 항목 설명 참조) — 이 화이트리스트는 보안 장치이면서 동시에
//! **상호 배제의 전제**다.
//!
//! 여기에 없는 것은 전부 자식에게 도달하지 않는다. 그중 의도적으로 뺀 것들:
//!
//! - `XB_CONFIG` — 대신 `--config <경로>`를 명시적으로 붙인다. env로 흘리면 서버가 보는
//!   config와 자식이 보는 config가 갈라질 수 있다.
//! - `XB_PROFILE` — 잡이 실행할 프로파일은 [`JobSpec`]이 정한다. 서버 셸에 남은
//!   `XB_PROFILE`이 `status`·`list`처럼 프로파일이 선택인 명령의 대상을 조용히 바꾸면,
//!   감사 로그에 적힌 argv와 실제로 점검된 프로파일이 달라진다.
//! - `XB_*__*` config 오버라이드([`crate::config::env`]) — 이름이 열려 있어 화이트리스트로
//!   열거할 수 없고, 값에 URI(=시크릿)가 들어올 수 있다. **더 중요한 이유는 감사
//!   가능성이다:** 잡의 동작이 "우리가 기록한 것"(argv + config + 명시적으로 주입한
//!   시크릿)만으로 결정되어야, 감사 로그를 보고 무슨 일이 있었는지 재구성할 수 있다.
//!   보이지 않는 env가 동작을 바꿀 수 있으면 감사 로그는 반쪽짜리 기록이 된다.
//!   대가는 정직하게 밝힌다 — 셸에 `XB_SOURCE__URI`를 export해 두고 CLI를 쓰던 운영자는
//!   콘솔에서 같은 잡이 설정 오류(exit 2)로 끊기는 것을 본다. 조용히 다르게 도는 것보다
//!   낫다.
//! - `RUST_LOG` — 잡의 로그 상세도가 서버 셸 환경으로 결정되면 안 된다(자식 stderr는
//!   브라우저로 중계된다 — t11).
//! - `XB_WEB_TOKEN`/`XB_WEB_TOKEN_FILE` — 웹 인증 토큰. 자식이 알 이유가 전혀 없다.
//!
//! ## 시크릿은 그 자식에게만, spawn 순간에만
//! 시크릿은 [`super::JobSecrets`]가 프로세스 메모리에 들고 있다가 이 파일이 자식 하나의
//! env에 넣는다. 서버 프로세스 환경에 상주시키지 않는 이유와 그 방법의 한계는
//! [`super::JobSecrets`] doc에 적었다.
//!
//! ## 자식은 서버의 종료를 따라 죽지 않는다 — `kill_on_drop`을 쓰지 않는다
//! [`crate::engine`]의 dump/restore와 [`crate::hooks`]는 `kill_on_drop(true)`를 켠다.
//! 여기서는 **의도적으로 켜지 않는다.** 2시간짜리 백업이 배포(SIGTERM)나 라우트 핸들러의
//! 조기 반환 때마다 죽으면 안 된다([`crate::web::server`] 헤더의 같은 결정). 자식은
//! 스스로 락을 들고 계속 돌고, 서버가 다시 뜨면 t12가 pid+시작시각+프로파일 3중 일치로
//! 재부착한다.
//!
//! **대가:** [`RunningJob`]을 `wait()` 없이 drop하면 자식은 계속 돌고, 끝난 뒤에는
//! 서버가 종료될 때까지 좀비(zombie)로 남는다. 그래서 잡 하나마다 반드시 누군가
//! [`RunningJob::wait`] 또는 [`RunningJob::wait_with_output`]을 호출해야 한다 —
//! 프로세스를 죽이는 것과 수거하는 것은 다른 일이고, 우리가 포기한 것은 앞쪽뿐이다.
//!
//! ## 자식을 자기 프로세스 그룹의 리더로 만든다(unix)
//! `process_group(0)`을 준다. 두 가지를 얻는다:
//! 1. 서버를 포그라운드에서 돌리다 Ctrl-C를 누르면 SIGINT는 포그라운드 프로세스
//!    **그룹**으로 간다 — 자식이 같은 그룹에 있으면 진행 중인 백업이 함께 죽는다.
//! 2. t12가 취소를 구현할 때 `kill(-pgid, SIGTERM)`으로 자식이 다시 띄운 손자
//!    (mongodump/pg_dump)까지 한 번에 정리할 수 있다. 직계 자식만 죽이면 dump 프로세스가
//!    고아로 남는다.
//!
//! ## stdin은 `/dev/null`이다
//! 자식은 절대 사람에게 물어볼 수 없다. `prune`의 대화형 확인과 `restore`의 fuzzy 선택은
//! TTY가 있을 때만 열리므로, stdin을 null로 두면 그 경로가 문법 수준에서 닫힌다. 그래서
//! 웹에서 `prune`을 실삭제로 돌리려면 `--force`가 **필수**다(없으면 자식이 비-TTY에서
//! 삭제를 거부하고 exit 1로 끊는다 — [`crate::cli::handlers::prune`] 헤더의 삭제 게이팅).
//!
//! ## 종료 코드 0~5를 뭉개지 않는다
//! [`JobOutcome`]은 PRD §9의 여섯 코드를 각각 다른 상태로 접고, 시그널 종료까지 따로
//! 둔다. 특히 **4(경고 동반 성공)를 실패로, 5(락 충돌)를 실패로 접으면 안 된다** —
//! 운영자의 대응이 완전히 다르다(각 variant의 doc 참조).
//!
//! ## 파괴적 작업은 [`AuditReceipt`] 없이는 실행할 수 없다
//! [`JobRunner::spawn`]은 파괴적 명령을 **거부**하고, [`JobRunner::spawn_destructive`]는
//! [`AuditReceipt`]를 **값으로** 요구한다. receipt는 [`crate::web::audit::AuditLog::gate`]가
//! append에 성공했을 때만 만들어지고 이 크레이트의 다른 어디서도 만들 수 없다(t8). 두
//! 함수를 합치면 "기록 없이 파괴적 작업을 실행하는" 코드 경로가 존재하지 않는다:
//! 타입으로 막힌 쪽(receipt 필요)과 런타임으로 막힌 쪽(`spawn`이 거부) 양방향이다.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::process::{Child, ChildStderr, ChildStdout, Command};

use crate::error::{exit_codes, Result, XBackupError};
use crate::i18n::Lang;
use crate::web::audit::{AuditOutcome, AuditReceipt};
use crate::web::job::spec::{JobCommand, JobSpec};
use crate::web::job::JobSecrets;
use crate::web::mask::SecretRegistry;

/// 자식에게 그대로 물려주는 env 변수 이름 — **이 목록에 없는 것은 도달하지 않는다.**
///
/// 각 항목의 근거(없으면 무엇이 깨지는가):
///
/// - `PATH` — 자식이 다시 띄우는 외부 도구(`mongodump`/`mongorestore`) 탐색. 없으면
///   그 도구를 못 찾아 잡이 실패한다. **서버 환경에 `PATH`가 없으면 우리가 기본값을
///   만들어 넣지 않는다** — 임의의 기본 `PATH`를 주면 웹이 찾는 도구와 cron으로 돌린
///   CLI가 찾는 도구가 달라질 수 있고, 그건 "웹과 CLI가 다르게 동작할 수 없다"는
///   불변식을 우리 손으로 깨는 것이다. 없으면 자식이 CLI와 똑같은 메시지
///   ("설치/PATH 확인")로 실패한다.
/// - `HOME` — 외부 도구가 홈 기준 설정을 찾는다(대표적으로 PostgreSQL의 `~/.pgpass`).
///   없으면 그 경로로 인증하던 프로파일이 웹에서만 실패한다.
/// - `TMPDIR` — 자식은 접속 URI를 argv 대신 0600 임시 config 파일로 넘긴다
///   ([`crate::engine`] dump 경로). 그 파일이 만들어지는 위치를 서버·cron·CLI가 공유해야
///   운영자가 한 곳만 보면 된다.
/// - `TZ` — 로그·표시 시각. 없으면 같은 잡의 시각 표기가 CLI와 달라진다.
/// - `LANG`/`LC_ALL`/`LC_CTYPE` — 외부 도구의 메시지 인코딩. 로케일이 어긋나면 도구가
///   비-UTF-8 바이트를 뱉고, 그 stderr를 파싱·중계하는 t11이 깨진다. 우리 자신의 출력
///   언어는 이 값이 아니라 `--lang`이 정한다([`JobRunner::build_command`]).
/// - `XDG_RUNTIME_DIR` — **가장 중요하다.** 파일 락 경로가
///   `$XDG_RUNTIME_DIR/x-backup/<profile>.lock`이고, 없으면 `/tmp/x-backup`으로
///   떨어진다([`crate::lock::file_lock`]). 이걸 빠뜨리면 웹이 띄운 자식은 `/tmp` 아래에,
///   cron으로 돌린 CLI는 `$XDG_RUNTIME_DIR` 아래에 락을 잡는다 — **서로를 보지 못하므로
///   같은 프로파일에 백업과 복구가 동시에 돌 수 있다.** 자식이 락을 상속한다는 이
///   아키텍처의 핵심 이점이 조용히 사라지는 지점이라, 이 한 줄이 화이트리스트에서 가장
///   비싼 항목이다.
/// - `HOSTNAME` — 락 파일에 기록되는 소유자 호스트명. 값이 다르면 stale 판정(다른
///   호스트의 락은 회수하지 않는다)이 다르게 동작한다.
///
/// 여기 없는 것들과 그 이유는 모듈 헤더 "자식 env는 화이트리스트로 새로 짓는다" 참조.
/// `XB_`로 시작하는 이름은 **하나도 없다** — x-backup 자신의 설정은 argv로 명시하거나
/// 시크릿으로 주입한다(아래 `whitelist_contains_no_xb_variables` 테스트가 고정한다).
pub const FORWARDED_ENV_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TZ",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "XDG_RUNTIME_DIR",
    "HOSTNAME",
];

/// 자식 프로세스 하나의 최종 상태 — PRD §9 종료 코드 규약을 그대로 접는다.
///
/// 여섯 코드를 각각 다른 variant로 두는 이유는 하나다: **운영자의 대응이 다르다.**
/// "성공/실패" 두 칸으로 뭉개면 화면이 거짓말을 한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// exit 0 — 성공.
    Succeeded,
    /// exit 1 — 작업 실패. 원인을 보고 고친 뒤 다시 돌려야 한다.
    Failed,
    /// exit 2 — 사용법·설정 오류. **작업은 시작되지 않았다.** 데이터는 그대로다.
    /// 웹에서 이게 나오면 대개 명세 조합이 잘못됐다는 뜻이다(어느 플래그가 어느 명령에
    /// 유효한지는 CLI가 판정한다 — [`super::spec::JobFlag`] doc).
    Rejected,
    /// exit 3 — 사전 점검 실패. **작업은 시작되지 않았다.** 대상 서버·설정을 먼저
    /// 고쳐야 하며, 재시도해도 같은 결과가 나온다.
    PrecheckFailed,
    /// exit 4 — **경고를 동반한 성공.** 작업은 끝났고 산출물이 있다(oplog gap으로
    /// 증분→풀 승격, verify 경고 등). 이것을 실패로 접으면 운영자가 멀쩡한 백업을
    /// 다시 돌리거나, 반대로 경고를 놓친다.
    SucceededWithWarnings,
    /// exit 5 — 잠금 충돌. **아무 일도 일어나지 않았다.** 같은 프로파일의 다른
    /// 인스턴스(cron이 띄운 CLI일 수도 있다)가 돌고 있다는 뜻이므로, 고칠 것은 없고
    /// 그 작업이 끝난 뒤 그대로 다시 시도하면 된다([`JobOutcome::retryable`]).
    LockConflict,
    /// PRD가 정의하지 않은 종료 코드. 자식이 우리 에러 모델을 거치지 않고 죽은
    /// 경우다(panic → abort 등) — 삼키지 않고 코드를 그대로 보존한다.
    UnexpectedExit(i32),
    /// 시그널로 종료되어 종료 코드가 없다(unix). 대개 취소(SIGTERM), OOM 킬(SIGKILL),
    /// 세그폴트다. **실패와 구분해야 한다** — 잡 자신이 판정한 결과가 아니라 밖에서
    /// 끊긴 것이므로, 산출물이 반쯤 남았을 수 있다.
    Signaled(i32),
    /// 종료 코드도 시그널 번호도 알 수 없는 상태(비-unix 플랫폼의 예외 경로).
    /// 억지로 성공/실패로 접지 않는다.
    Unknown,
}

impl JobOutcome {
    /// 프로세스 종료 상태를 분류한다.
    fn from_status(status: std::process::ExitStatus) -> Self {
        if let Some(code) = status.code() {
            // PRD 코드는 0~5이므로 u8로 접어 상수 패턴과 직접 맞춘다. 범위를 벗어나면
            // 원래 i32를 그대로 보존한다(뭉개면 진단이 사라진다).
            return match u8::try_from(code) {
                Ok(exit_codes::SUCCESS) => Self::Succeeded,
                Ok(exit_codes::FAILURE) => Self::Failed,
                Ok(exit_codes::USAGE) => Self::Rejected,
                Ok(exit_codes::PRECHECK) => Self::PrecheckFailed,
                Ok(exit_codes::WARNING) => Self::SucceededWithWarnings,
                Ok(exit_codes::LOCK_CONFLICT) => Self::LockConflict,
                _ => Self::UnexpectedExit(code),
            };
        }
        // 종료 코드가 없다 = 시그널로 죽었다(unix).
        #[cfg(unix)]
        {
            use std::os::unix::process::ExitStatusExt;
            if let Some(signal) = status.signal() {
                return Self::Signaled(signal);
            }
        }
        Self::Unknown
    }

    /// 종료 코드(있을 때). 시그널 종료·미상은 `None`이다.
    pub fn exit_code(&self) -> Option<i32> {
        match self {
            Self::Succeeded => Some(i32::from(exit_codes::SUCCESS)),
            Self::Failed => Some(i32::from(exit_codes::FAILURE)),
            Self::Rejected => Some(i32::from(exit_codes::USAGE)),
            Self::PrecheckFailed => Some(i32::from(exit_codes::PRECHECK)),
            Self::SucceededWithWarnings => Some(i32::from(exit_codes::WARNING)),
            Self::LockConflict => Some(i32::from(exit_codes::LOCK_CONFLICT)),
            Self::UnexpectedExit(code) => Some(*code),
            Self::Signaled(_) | Self::Unknown => None,
        }
    }

    /// 작업이 완료됐는지 — exit 0과 exit 4만 참이다.
    ///
    /// **exit 4는 성공이다**(경고 동반). 이 함수가 4를 false로 돌리면 화면이 멀쩡한
    /// 백업을 실패로 표시하고, 운영자가 이미 있는 백업을 다시 돌린다.
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Succeeded | Self::SucceededWithWarnings)
    }

    /// 작업이 실제로 **시작됐는지**.
    ///
    /// exit 2/3/5는 작업 이전 단계에서 끊긴 것이므로 데이터에 아무 변화가 없다. 화면이
    /// "실패"와 "시작조차 안 함"을 구분해야 운영자가 부분 산출물을 찾아 헤매지 않는다.
    pub fn started(&self) -> bool {
        !matches!(
            self,
            Self::Rejected | Self::PrecheckFailed | Self::LockConflict
        )
    }

    /// 그대로 다시 시도하면 될 상태인지 — 잠금 충돌만 참이다.
    ///
    /// 다른 실패는 원인(설정·대상 상태·데이터)을 고쳐야 하므로 자동 재시도가 무의미하거나
    /// 해롭다. 잠금 충돌은 "지금 다른 인스턴스가 쓰고 있다"는 뜻일 뿐이다.
    pub fn retryable(&self) -> bool {
        matches!(self, Self::LockConflict)
    }

    /// 화면·API에 쓰는 영문 라벨(라벨은 항상 영문 — [`crate::i18n`] 규약).
    pub fn label(&self) -> &'static str {
        match self {
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Rejected => "rejected",
            Self::PrecheckFailed => "precheck-failed",
            Self::SucceededWithWarnings => "succeeded-with-warnings",
            Self::LockConflict => "lock-conflict",
            Self::UnexpectedExit(_) => "unexpected-exit",
            Self::Signaled(_) => "signaled",
            Self::Unknown => "unknown",
        }
    }

    /// 감사 로그의 결과 상태로 접는다(t8 [`AuditOutcome`]).
    ///
    /// t8의 어휘는 `Success`/`Failure` 둘뿐이라 여기서 정보가 줄어든다. 그래도 괜찮은
    /// 이유: 감사 항목에는 `exit_code`가 함께 남으므로, 나중에 로그를 훑을 때 4와 5는
    /// 코드로 구분된다. 이 함수는 "감사관이 먼저 보는 한 칸"만 정하는 것이다.
    pub fn audit_outcome(&self) -> AuditOutcome {
        if self.is_success() {
            AuditOutcome::Success
        } else {
            AuditOutcome::Failure
        }
    }
}

/// 돌고 있는 자식을 나중에 다시 찾기 위한 최소 정보.
///
/// t12(취소·고아 재부착)가 이 세 값을 state 디렉터리에 남긴다. **pid 하나만으로는
/// 안 된다** — pid는 재사용되므로, 서버가 재시작한 뒤 그 pid가 살아 있다고 해서 우리
/// 자식이라는 보장이 없다. `started_at`(시작 시각)과 `profile`을 함께 맞춰야
/// "그 pid가 정말 우리가 띄운 그 잡인가"를 판정할 수 있다([`crate::lock::LockData`]가
/// pid+started_at+hostname 3중으로 락 소유자를 판정하는 것과 같은 이유다).
#[derive(Debug, Clone, Serialize)]
pub struct JobHandle {
    /// 자식 pid. 이미 수거된 뒤에는 `None`이다.
    pub pid: Option<u32>,
    /// spawn 직전에 이 프로세스가 찍은 시각(UTC).
    ///
    /// 자식 자신이 아니라 **부모가** 찍는다 — 자식의 시작 시각을 자식에게 물으면
    /// 그 값을 얻기 위해 자식이 살아 있어야 하고, 죽은 뒤에는 확인할 방법이 없다.
    pub started_at: DateTime<Utc>,
    /// 서브커맨드 이름(`backup`/`restore`/…).
    pub command: &'static str,
    /// 대상 프로파일(있으면).
    pub profile: Option<String>,
}

/// spawn된 자식 하나의 핸들.
///
/// stdout/stderr는 파이프로 잡혀 있고 [`take_stdout`](Self::take_stdout)·
/// [`take_stderr`](Self::take_stderr)로 꺼낸다 — **NDJSON 파싱과 SSE 중계는 t11의
/// 몫이고 이 파일은 스트림을 넘겨주는 데까지만 한다.**
///
/// ## 파이프를 비우지 않으면 자식이 멈춘다
/// 파이프 버퍼(대개 64KB)가 차면 자식의 `write`가 블록된다. 그래서 스트림을 꺼내
/// 계속 읽지 않은 채 [`wait`](Self::wait)만 부르면 **교착**한다(자식은 쓰기에서 멈춰
/// 끝나지 않고, 우리는 끝나기를 기다린다). 두 갈래로 쓴다:
///
/// - 길게 도는 잡(backup/restore) → [`take_stdout`](Self::take_stdout)·
///   [`take_stderr`](Self::take_stderr)로 꺼내 **독립 태스크에서 끝까지 읽으면서**
///   [`wait`](Self::wait)를 기다린다(t11).
/// - 짧게 끝나는 잡(list/status/doctor) → [`wait_with_output`](Self::wait_with_output)
///   하나로 끝낸다(내부에서 두 파이프를 동시에 비운다).
pub struct RunningJob {
    child: Child,
    handle: JobHandle,
    argv: Vec<String>,
}

impl RunningJob {
    /// stdout 파이프를 꺼낸다(한 번만 성공한다).
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }

    /// stderr 파이프를 꺼낸다(한 번만 성공한다).
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }

    /// 재부착·취소에 필요한 식별 정보([`JobHandle`]).
    pub fn handle(&self) -> &JobHandle {
        &self.handle
    }

    /// 자식 pid — t12가 `SIGTERM`을 보낼 대상. unix에서는 자식이 자기 프로세스 그룹의
    /// 리더이므로(모듈 헤더), `kill(-pid, SIGTERM)`으로 손자까지 함께 정리할 수 있다.
    pub fn pid(&self) -> Option<u32> {
        self.handle.pid
    }

    /// spawn 시각(UTC).
    pub fn started_at(&self) -> DateTime<Utc> {
        self.handle.started_at
    }

    /// 실제로 넘긴 argv(전역 플래그 제외 — [`JobSpec::to_argv`]가 만든 부분).
    ///
    /// 시크릿이 들어갈 자리가 없는 어휘이므로 그대로 로그·화면에 쓸 수 있다
    /// ([`super::spec`] 헤더 "웹은 접속 URI를 인자로 받지 않는다").
    pub fn argv(&self) -> &[String] {
        &self.argv
    }

    /// 자식이 끝나기를 기다려 종료 상태를 분류한다.
    ///
    /// 파이프를 비우지 않은 채 부르면 교착할 수 있다 — 위 "파이프를 비우지 않으면
    /// 자식이 멈춘다" 참조.
    pub async fn wait(&mut self) -> Result<JobOutcome> {
        let status = self.child.wait().await.map_err(|e| {
            XBackupError::Failure(format!(
                "잡 자식 프로세스({})의 종료를 기다리지 못했습니다: {e}",
                self.handle.command
            ))
        })?;
        // 수거가 끝난 뒤의 pid는 더 이상 이 잡을 가리키지 않는다(재사용 가능) —
        // 남겨 두면 t12가 죽은 pid에 시그널을 보낼 수 있다.
        self.handle.pid = None;
        Ok(JobOutcome::from_status(status))
    }

    /// stdout/stderr를 끝까지 모으면서 종료를 기다린다 — 짧게 끝나는 잡 전용.
    ///
    /// 출력을 **전부 메모리에 담는다.** 백업처럼 진행 로그가 계속 나오는 잡에는 쓰지
    /// 말 것(그건 t11의 스트리밍 경로다).
    ///
    /// 이미 [`take_stdout`](Self::take_stdout)/[`take_stderr`](Self::take_stderr)로 꺼낸
    /// 스트림은 여기서 빈 문자열이 된다 — 파이프의 주인은 하나뿐이다.
    pub async fn wait_with_output(self) -> Result<JobCompletion> {
        let command = self.handle.command;
        let output = self.child.wait_with_output().await.map_err(|e| {
            XBackupError::Failure(format!(
                "잡 자식 프로세스({command})의 출력을 수집하지 못했습니다: {e}"
            ))
        })?;
        Ok(JobCompletion {
            outcome: JobOutcome::from_status(output.status),
            // 자식이 비-UTF-8 바이트를 뱉을 수 있다(외부 도구의 로케일 의존 메시지).
            // 여기서 실패로 끊으면 종료 코드를 잃으므로 lossy 변환한다 — 판정에 쓰는
            // 것은 종료 코드이고, 문자열은 사람이 읽을 진단이다.
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

impl std::fmt::Debug for RunningJob {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Child는 Debug를 구현하지만 내부 fd를 노출한다 — 진단에 필요한 것만 보여준다.
        f.debug_struct("RunningJob")
            .field("handle", &self.handle)
            .field("argv", &self.argv)
            .finish()
    }
}

/// [`RunningJob::wait_with_output`]의 결과.
#[derive(Debug, Clone)]
pub struct JobCompletion {
    /// 분류된 종료 상태.
    pub outcome: JobOutcome,
    /// 자식 stdout 전체(`--json` 출력).
    pub stdout: String,
    /// 자식 stderr 전체(사람용 진단·경고).
    pub stderr: String,
}

/// 잡 자식을 띄우는 공장.
///
/// 기동 시점에 한 번 만들어 [`crate::web::ServeConfig`]에 실어 두고, 라우트 핸들러가
/// 공유해 쓴다. 여기에 담긴 것(자기 바이너리 경로·config 경로·env 화이트리스트 스냅샷·
/// 시크릿)은 전부 **기동 시점에 확정**되며 요청마다 바뀌지 않는다 — 요청이 바꿀 수 있는
/// 것은 [`JobSpec`]뿐이다.
pub struct JobRunner {
    /// 실행할 바이너리 = 우리 자신([`std::env::current_exe`]).
    exe: PathBuf,
    /// 자식에게 `--config`로 넘길 경로(없으면 자식이 CLI와 같은 규칙으로 자동 탐색한다).
    config_path: Option<PathBuf>,
    /// 자식 출력 설명 언어(`--lang`).
    lang: Lang,
    /// 화이트리스트로 걸러 스냅샷한 base env.
    ///
    /// 기동 시점에 한 번 읽는다 — 요청마다 프로세스 환경을 다시 읽으면, 서버가 뜬 뒤
    /// 누군가 env를 바꿨을 때 잡마다 다른 환경에서 돌 수 있다. 기동 시점 고정이
    /// "이 서버가 띄우는 모든 잡은 같은 환경에서 돈다"를 보장한다.
    base_env: Vec<(String, String)>,
    /// 자식에게만 넣는 시크릿.
    secrets: JobSecrets,
}

impl std::fmt::Debug for JobRunner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // base_env의 **값**은 찍지 않는다. 화이트리스트에 시크릿은 없지만, `PATH`·`HOME`
        // 같은 값도 서버 배치 구조를 드러내는 정보이고 진단에 필요한 것은 "무엇이
        // 전달되는가"(이름)다. secrets는 자신의 Debug가 개수만 보여준다.
        let forwarded: Vec<&str> = self.base_env.iter().map(|(k, _)| k.as_str()).collect();
        f.debug_struct("JobRunner")
            .field("exe", &self.exe)
            .field("config_path", &self.config_path)
            .field("lang", &self.lang)
            .field("forwarded_env", &forwarded)
            .field("secrets", &self.secrets)
            .finish()
    }
}

impl JobRunner {
    /// 러너를 만든다 — 실행 대상은 [`std::env::current_exe`], base env는 화이트리스트
    /// 스냅샷.
    ///
    /// `current_exe()` 실패는 설정 오류(exit 2)로 올린다. 자기 경로를 모르면 잡을 하나도
    /// 띄울 수 없으므로, 그 상태로 서버를 띄우는 것은 "버튼이 전부 죽은 콘솔"을 만드는
    /// 것이다 — 기동 시점에 끊는 편이 낫다(fail-closed).
    pub fn new(config_path: Option<PathBuf>, lang: Lang, secrets: JobSecrets) -> Result<Self> {
        let exe = std::env::current_exe().map_err(|e| {
            XBackupError::Config(format!(
                "실행 중인 x-backup 바이너리 경로를 확인할 수 없습니다: {e} — 잡을 띄울 수 \
                 없으므로 서버를 기동하지 않습니다."
            ))
        })?;
        Ok(Self {
            exe,
            config_path,
            lang,
            base_env: snapshot_forwarded_env(),
            secrets,
        })
    }

    /// 실행 대상 바이너리를 직접 지정해 러너를 만든다 — **테스트 전용**.
    ///
    /// 실제 기동 경로는 항상 [`JobRunner::new`]다(모듈 헤더의 `current_exe` 근거). 이
    /// 생성자는 (1) `/usr/bin/env`·`/bin/echo` 같은 관찰용 프로그램을 띄워 자식 env와
    /// argv 전달을 **직접 확인**하고, (2) 라이브러리 테스트 하니스에서 실제
    /// `x-backup` 바이너리를 가리키기 위해 존재한다(`current_exe()`는 테스트에서 테스트
    /// 하니스 자신을 가리킨다).
    #[cfg(test)]
    pub(crate) fn with_exe(
        exe: PathBuf,
        config_path: Option<PathBuf>,
        lang: Lang,
        secrets: JobSecrets,
    ) -> Self {
        Self {
            exe,
            config_path,
            lang,
            base_env: snapshot_forwarded_env(),
            secrets,
        }
    }

    /// 실행 대상 바이너리 경로.
    pub fn program(&self) -> &Path {
        &self.exe
    }

    /// 자식에게 전달되는 env 변수 이름(값은 노출하지 않는다) — 진단·테스트용.
    pub fn forwarded_env_names(&self) -> Vec<&str> {
        self.base_env.iter().map(|(k, _)| k.as_str()).collect()
    }

    /// 자식 출력에서 시크릿을 지우는 데 쓸 레지스트리(t11이 stdout/stderr 중계 전에
    /// 통과시킨다 — [`crate::web::mask`]).
    pub fn secret_registry(&self) -> &SecretRegistry {
        self.secrets.registry()
    }

    /// 주입은 됐지만 마스킹 대상으로 등록되지 못한 env 이름들 —
    /// [`JobSecrets::unmaskable_env_names`]를 그대로 노출한다.
    ///
    /// 기동 배너([`crate::web::server`])가 이 목록으로 운영자에게 경고한다. 러너를 통해
    /// 노출하는 이유: `ServeConfig`가 들고 있는 것은 `JobSecrets`가 아니라 `JobRunner`다
    /// (시크릿 사본을 늘리지 않으려고 `JobSecrets`는 러너가 소유한다 — 그 타입 doc).
    pub fn unmaskable_env_names(&self) -> &[String] {
        self.secrets.unmaskable_env_names()
    }

    /// **비파괴** 잡을 띄운다.
    ///
    /// 파괴적 명령(restore/prune/migrate)은 거부한다 — 그쪽은 감사 게이트를 지나
    /// [`spawn_destructive`](Self::spawn_destructive)로 와야 한다. 타입만으로는
    /// "파괴적 명령을 담은 [`JobSpec`]"과 그렇지 않은 것을 구분할 수 없으므로(명령은
    /// 런타임 값이다), 이 런타임 검사가 타입 검사의 빈틈을 메운다.
    pub fn spawn(&self, spec: &JobSpec) -> Result<RunningJob> {
        if spec.is_destructive() {
            return Err(XBackupError::Usage(format!(
                "'{}'은 파괴적 명령이므로 감사 기록 없이 실행할 수 없습니다 — \
                 AuditLog::gate()로 AuditReceipt를 받은 뒤 spawn_destructive()를 쓰세요.",
                spec.command().verb()
            )));
        }
        self.spawn_inner(spec)
    }

    /// **계획만 내는(`--dry-run`) 잡**을 띄운다 — 파괴적 명령이라도 받는다.
    ///
    /// ## 왜 별도 진입점인가
    /// [`spawn`](Self::spawn)의 거부는 **명령 단위**다(`prune`은 언제나 파괴적으로 본다).
    /// 그 판정은 옳지만, `prune --dry-run`은 계획을 stdout에 찍을 뿐 아무것도 지우지 않는다.
    /// 그런데 웹의 삭제 화면은 **계획을 먼저 보여주는 것이 안전장치의 핵심**이다
    /// ([`crate::web::routes::prune`] 헤더) — 그 계획을 얻으려고 감사 게이트를 통과해야
    /// 한다면 두 가지가 무너진다:
    ///
    /// 1. **감사 로그가 거짓말한다.** 게이트 기록은 "이 파괴적 작업이 승인되어 실행 직전이다"
    ///    라는 뜻인데, 미리보기마다 그 줄이 찍히면 로그를 읽는 사람이 실행과 조회를 구분할 수
    ///    없다. 그러면 진짜 삭제 기록이 미리보기 속에 묻힌다.
    /// 2. **순서가 뒤집힌다.** 확인 화면은 계획을 본 **뒤에** 나와야 하는데, 게이트를 먼저
    ///    통과해야 계획을 얻는다면 "무엇이 지워지는지 모른 채 승인"이 된다.
    ///
    /// ## 이 진입점이 넓히지 않는 것
    /// `spec`에 `--dry-run`이 실제로 실려 있는지 [`JobSpec::has_dry_run`]으로 **런타임에
    /// 확인**하고, 없으면 거부한다. 즉 이 함수로는 무언가를 지우는 argv를 띄울 수 없다.
    /// 감사 게이트를 우회하는 구멍이 아니라, "지우지 않는 실행"만 통과시키는 좁은 문이다.
    ///
    /// 미리보기를 감사에 남기지 않는 것은 [`crate::web::guard`] 헤더의 판단과도 일관된다 —
    /// 그 모듈도 "확인 화면을 **그리는 것**은 아직 시도가 아니다"라며 렌더를 기록하지 않는다.
    pub fn spawn_preview(&self, spec: &JobSpec) -> Result<RunningJob> {
        if !spec.has_dry_run() {
            return Err(XBackupError::Usage(format!(
                "'{}'을 미리보기로 띄우려면 --dry-run이 있어야 합니다 — 이 진입점은                  아무것도 바꾸지 않는 실행만 받습니다. 실제 실행은 AuditLog::gate()로                  AuditReceipt를 받은 뒤 spawn_destructive()를 쓰세요.",
                spec.command().verb()
            )));
        }
        self.spawn_inner(spec)
    }

    /// **파괴적** 잡을 띄운다 — [`AuditReceipt`]를 값으로 소비한다.
    ///
    /// receipt는 [`crate::web::audit::AuditLog::gate`]가 감사 로그 append에 성공했을 때만
    /// 만들어진다(t8). 이 시그니처 덕분에 "기록을 건너뛰고 복구를 실행하는" 코드는 넘길
    /// 값을 만들 방법이 없어 **컴파일되지 않는다.** receipt는 `Clone`이 아니므로 한 번
    /// 쓰면 끝이고, 두 번째 파괴적 실행은 자신의 `gate()` 호출로 새 receipt를 받아야 한다.
    ///
    /// 비파괴 명세를 넘기는 것도 허용한다 — 과다 기록은 안전한 방향의 실패다
    /// ([`super::spec`] 헤더).
    pub fn spawn_destructive(&self, spec: &JobSpec, receipt: AuditReceipt) -> Result<RunningJob> {
        // receipt를 여기서 소비한다. 값을 읽지는 않는다 — 읽을 것이 없다(필드가 없다).
        // 이 값의 존재 자체가 이 함수의 전제조건이고, `Clone`이 아니므로 소비되는 순간
        // 같은 receipt로 두 번째 파괴적 실행을 할 수 없게 된다(t8의 설계).
        let _receipt: AuditReceipt = receipt;
        self.spawn_inner(spec)
    }

    /// argv를 그대로 넘겨 자식을 띄운다 — **테스트 전용**. 전역 플래그(`--config`/
    /// `--lang`)도 붙이지 않는다.
    ///
    /// 두 가지를 관찰하기 위해 존재한다:
    ///
    /// 1. **셸을 경유하지 않는다는 성질.** 이 성질은 "검증에 걸리는 문자열이 argv에
    ///    도달했을 때에도" 성립해야 하는데, [`JobSpec`]이 그런 문자열을 애초에 거부하므로
    ///    정상 경로로는 관찰할 수 없다.
    /// 2. **자식이 실제로 받은 env.** `/usr/bin/env`처럼 환경을 찍어 주는 프로그램은
    ///    우리 전역 플래그를 자기 옵션으로 해석해 실패하므로(`env --lang` = 알 수 없는
    ///    옵션), 전역 플래그 없이 띄울 경로가 필요하다.
    #[cfg(test)]
    pub(crate) fn spawn_raw(&self, argv: Vec<String>) -> Result<RunningJob> {
        let mut cmd = self.base_command();
        for arg in &argv {
            cmd.arg(arg);
        }
        self.spawn_prepared(cmd, JobCommand::Doctor, None, argv)
    }

    /// [`spawn`](Self::spawn)/[`spawn_destructive`](Self::spawn_destructive) 공통 본체.
    fn spawn_inner(&self, spec: &JobSpec) -> Result<RunningJob> {
        self.spawn_argv(
            spec.command(),
            spec.profile().map(|p| p.as_str().to_string()),
            spec.to_argv(),
        )
    }

    /// 명세로부터 [`Command`]를 조립해 spawn한다.
    fn spawn_argv(
        &self,
        command: JobCommand,
        profile: Option<String>,
        argv: Vec<String>,
    ) -> Result<RunningJob> {
        let cmd = self.build_command(&argv);
        self.spawn_prepared(cmd, command, profile, argv)
    }

    /// 실제 spawn — 여기가 이 파일의 유일한 프로세스 생성 지점이다.
    fn spawn_prepared(
        &self,
        mut cmd: Command,
        command: JobCommand,
        profile: Option<String>,
        argv: Vec<String>,
    ) -> Result<RunningJob> {
        // 시각을 spawn **직전**에 찍는다 — spawn이 실패하면 남길 시각도 없고, 성공했을
        // 때 이 값은 "자식이 존재하기 시작한 시각"의 하한이다(t12의 재부착 판정은 하한만
        // 필요하다: 그 시각 이후에 시작된 프로세스인지만 보면 pid 재사용을 걸러낸다).
        let started_at = Utc::now();
        let child = cmd.spawn().map_err(|e| {
            XBackupError::Failure(format!(
                "잡 프로세스를 시작할 수 없습니다({} {}): {e}",
                self.exe.display(),
                command.verb()
            ))
        })?;
        let pid = child.id();

        tracing::info!(
            command = command.verb(),
            profile = profile.as_deref().unwrap_or("-"),
            pid = pid.unwrap_or(0),
            "잡 자식 프로세스 시작"
        );

        Ok(RunningJob {
            child,
            handle: JobHandle {
                pid,
                started_at,
                command: command.verb(),
                profile,
            },
            argv,
        })
    }

    /// 인자가 하나도 없는 자식 [`Command`] — env 화이트리스트·시크릿 주입·표준 입출력·
    /// 프로세스 그룹까지 확정된 상태.
    ///
    /// "무엇을 실행할지"(인자)와 "어떤 환경에서 실행할지"(여기)를 나눠 둔다 — 환경 구성은
    /// 모든 경로가 공유해야 하는 보안 성질이고, 인자는 경로마다 다르다.
    fn base_command(&self) -> Command {
        let mut cmd = Command::new(&self.exe);

        // 1) 서버 환경을 통째로 버린다. 이 한 줄이 화이트리스트 방식의 전부다 —
        //    이 뒤에 넣는 것만 자식에게 존재한다(모듈 헤더 참조).
        cmd.env_clear();
        for (name, value) in &self.base_env {
            cmd.env(name, value);
        }
        // 2) 시크릿은 이 자식에게만.
        self.secrets.apply(&mut cmd);

        // 3) 표준 입출력. stdin=null은 대화형 경로를 문법 수준에서 닫는다(모듈 헤더).
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // 4) 자기 프로세스 그룹의 리더로 만든다(모듈 헤더). `kill_on_drop`은
        //    **의도적으로 켜지 않는다.**
        #[cfg(unix)]
        cmd.process_group(0);

        cmd
    }

    /// [`base_command`](Self::base_command)에 전역 플래그와 잡 argv를 얹는다.
    fn build_command(&self, argv: &[String]) -> Command {
        let mut cmd = self.base_command();

        // 전역 플래그는 서브커맨드 **앞**에 놓는다. clap의 global 인자는 뒤에 와도
        // 받아들여지지만, 앞에 두면 `x-backup --config <p> --lang en backup …`처럼
        // 사람이 손으로 재현할 수 있는 형태가 되어 감사 로그·진단이 읽기 쉬워진다.
        if let Some(path) = &self.config_path {
            cmd.arg("--config").arg(path);
        }
        cmd.arg("--lang").arg(lang_arg(self.lang));

        // 잡 argv. 각 원소를 개별 `arg()`로 넣는다 — 문자열을 이어 붙이지 않으므로
        // 공백·메타문자가 인자 경계를 만들 수 없다(모듈 헤더 "셸을 경유하지 않는다").
        for arg in argv {
            cmd.arg(arg);
        }

        cmd
    }
}

/// [`Lang`]을 `--lang` 값으로 접는다.
///
/// [`crate::i18n::Lang`]에 문자열 변환이 없어 여기서 매핑한다(i18n 모듈은 이 태스크
/// 범위 밖이다). clap의 `ValueEnum`이 받는 어휘와 같아야 하므로 아래
/// `lang_arg_round_trips_through_clap` 테스트가 두 쪽을 묶어 둔다.
fn lang_arg(lang: Lang) -> &'static str {
    match lang {
        Lang::En => "en",
        Lang::Ko => "ko",
    }
}

/// 현재 프로세스 환경에서 [`FORWARDED_ENV_NAMES`]에 해당하는 것만 골라 스냅샷한다.
///
/// 값이 비어 있는 변수는 넣지 않는다 — 빈 `PATH`나 빈 `TMPDIR`은 "설정되지 않음"과
/// 다르게 취급되어 도구를 엉뚱하게 동작시킬 수 있고(예: 빈 `PATH`는 현재 디렉터리를
/// 뜻하는 구현이 있다), [`crate::web::state_dir_from`]이 빈 `XDG_STATE_HOME`을 미설정으로
/// 보는 것과 같은 판단이다.
fn snapshot_forwarded_env() -> Vec<(String, String)> {
    FORWARDED_ENV_NAMES
        .iter()
        .filter_map(|name| {
            std::env::var(name)
                .ok()
                .filter(|v| !v.is_empty())
                .map(|v| ((*name).to_string(), v))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::job::spec::JobFlag;
    use crate::web::job::ProfileName;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// 종료 코드 `code`로 끝난 프로세스 상태를 만든다(unix `wait` 인코딩: 상위 8비트).
    fn status_code(code: i32) -> ExitStatus {
        ExitStatus::from_raw(code << 8)
    }

    /// 시그널 `signal`로 죽은 프로세스 상태를 만든다(하위 7비트).
    fn status_signal(signal: i32) -> ExitStatus {
        ExitStatus::from_raw(signal)
    }

    /// 관찰용 외부 프로그램 경로 — 없으면 테스트를 건너뛴다.
    fn observer(path: &str) -> Option<PathBuf> {
        let p = PathBuf::from(path);
        if p.exists() {
            Some(p)
        } else {
            eprintln!("건너뜀: {path}가 없는 환경입니다");
            None
        }
    }

    // ---- 종료 코드 분류 ----

    /// exit 0~5가 **서로 다른** 상태로 분류된다(PRD §9 여섯 코드).
    #[test]
    fn exit_codes_zero_to_five_map_to_distinct_states() {
        let mapped = [
            (0, JobOutcome::Succeeded),
            (1, JobOutcome::Failed),
            (2, JobOutcome::Rejected),
            (3, JobOutcome::PrecheckFailed),
            (4, JobOutcome::SucceededWithWarnings),
            (5, JobOutcome::LockConflict),
        ];
        for (code, expected) in mapped {
            assert_eq!(
                JobOutcome::from_status(status_code(code)),
                expected,
                "exit {code} 분류가 다르다"
            );
            assert_eq!(expected.exit_code(), Some(code));
        }
        // 여섯 상태가 실제로 서로 다른 값인지(하나로 접히지 않았는지) 확인한다.
        let mut labels: Vec<&str> = mapped.iter().map(|(_, o)| o.label()).collect();
        labels.sort_unstable();
        let count = labels.len();
        labels.dedup();
        assert_eq!(labels.len(), count, "서로 다른 코드가 같은 상태로 접혔다");
    }

    /// **exit 4는 실패가 아니다** — 경고 동반 성공.
    #[test]
    fn exit_four_is_success_not_failure() {
        let outcome = JobOutcome::from_status(status_code(4));
        assert_eq!(outcome, JobOutcome::SucceededWithWarnings);
        assert!(outcome.is_success(), "exit 4를 실패로 접으면 안 됨");
        assert!(outcome.started(), "exit 4는 작업이 끝난 상태다");
        assert!(
            !outcome.retryable(),
            "이미 성공했으므로 재시도 대상이 아니다"
        );
        assert_eq!(outcome.audit_outcome(), AuditOutcome::Success);
        // exit 1과 확실히 구분된다.
        assert_ne!(outcome, JobOutcome::from_status(status_code(1)));
    }

    /// **exit 5는 실패가 아니다** — 아무 일도 일어나지 않았고, 그대로 재시도하면 된다.
    #[test]
    fn exit_five_is_lock_conflict_not_failure() {
        let outcome = JobOutcome::from_status(status_code(5));
        assert_eq!(outcome, JobOutcome::LockConflict);
        assert!(!outcome.is_success());
        assert!(!outcome.started(), "락 충돌은 작업이 시작되지 않은 상태다");
        assert!(outcome.retryable(), "락 충돌만 재시도 대상이어야 함");
        // 일반 실패(exit 1)와 구분된다 — 운영자의 대응이 완전히 다르다.
        let failed = JobOutcome::from_status(status_code(1));
        assert_ne!(outcome, failed);
        assert!(failed.started());
        assert!(!failed.retryable());
    }

    /// exit 2/3은 "시작되지 않음"으로, exit 1은 "시작했으나 실패"로 갈린다.
    #[test]
    fn not_started_states_are_distinguished_from_failure() {
        assert!(!JobOutcome::from_status(status_code(2)).started());
        assert!(!JobOutcome::from_status(status_code(3)).started());
        assert!(JobOutcome::from_status(status_code(1)).started());
    }

    /// PRD 밖의 종료 코드는 코드를 보존한 채 별도 상태가 된다.
    #[test]
    fn unexpected_exit_code_is_preserved() {
        for code in [6, 42, 101, 127, 255] {
            let outcome = JobOutcome::from_status(status_code(code));
            assert_eq!(outcome, JobOutcome::UnexpectedExit(code));
            assert_eq!(outcome.exit_code(), Some(code));
            assert!(!outcome.is_success());
        }
    }

    /// 시그널 종료는 종료 코드가 없는 **별도 상태**다 — 실패로 접지 않는다.
    #[test]
    fn signal_termination_is_its_own_state() {
        for signal in [
            libc::SIGTERM, // t12의 취소 경로
            libc::SIGKILL, // OOM 킬
            libc::SIGSEGV,
        ] {
            let outcome = JobOutcome::from_status(status_signal(signal));
            assert_eq!(outcome, JobOutcome::Signaled(signal));
            assert_eq!(
                outcome.exit_code(),
                None,
                "시그널 종료에는 종료 코드가 없다"
            );
            assert!(!outcome.is_success());
            assert_eq!(outcome.label(), "signaled");
        }
        // 어떤 종료 코드와도 같지 않다.
        assert_ne!(
            JobOutcome::from_status(status_signal(libc::SIGTERM)),
            JobOutcome::from_status(status_code(1))
        );
    }

    /// 감사 결과는 성공(0·4)/실패(그 외)로 접히고, 종료 코드가 정보를 보존한다.
    #[test]
    fn audit_outcome_folds_only_the_first_column() {
        for code in [0, 4] {
            assert_eq!(
                JobOutcome::from_status(status_code(code)).audit_outcome(),
                AuditOutcome::Success
            );
        }
        for code in [1, 2, 3, 5, 99] {
            let outcome = JobOutcome::from_status(status_code(code));
            assert_eq!(outcome.audit_outcome(), AuditOutcome::Failure);
            assert_eq!(
                outcome.exit_code(),
                Some(code),
                "감사 항목에 남길 종료 코드가 보존되어야 함"
            );
        }
    }

    // ---- current_exe ----

    /// [`JobRunner::new`]는 실행 대상을 [`std::env::current_exe`]에서 가져온다 —
    /// `PATH`의 다른 x-backup을 부르지 않는다.
    #[test]
    fn new_resolves_program_from_current_exe() {
        let runner = JobRunner::new(None, Lang::En, JobSecrets::new()).expect("러너 생성 실패");
        let expected = std::env::current_exe().expect("current_exe 실패");
        assert_eq!(
            runner.program(),
            expected.as_path(),
            "실행 대상이 current_exe()가 아니다"
        );
        // 이름만으로 부르지 않는다는 것을 절대 경로로 확인한다(PATH 탐색 여지 없음).
        assert!(
            runner.program().is_absolute(),
            "실행 대상이 절대 경로가 아니다: {:?}",
            runner.program()
        );
    }

    // ---- env 화이트리스트 ----

    /// 화이트리스트에 `XB_*`(x-backup 자신의 설정·시크릿 이름 공간)는 하나도 없다.
    #[test]
    fn whitelist_contains_no_xb_variables() {
        for name in FORWARDED_ENV_NAMES {
            assert!(
                !name.starts_with("XB_"),
                "'{name}'이 화이트리스트에 있다 — x-backup 설정은 argv나 시크릿 주입으로 \
                 전달해야 한다"
            );
        }
    }

    /// 락 상호 배제의 전제인 `XDG_RUNTIME_DIR`·`HOSTNAME`이 화이트리스트에 있다.
    ///
    /// 이 두 줄이 빠지면 웹 자식과 cron CLI가 서로 다른 락 파일을 잡아 상호 배제가
    /// 조용히 사라진다([`FORWARDED_ENV_NAMES`] doc).
    #[test]
    fn whitelist_keeps_lock_related_variables() {
        for required in ["XDG_RUNTIME_DIR", "HOSTNAME", "PATH"] {
            assert!(
                FORWARDED_ENV_NAMES.contains(&required),
                "'{required}'가 화이트리스트에서 빠졌다"
            );
        }
    }

    /// 실제로 자식을 띄워 env를 관찰한다 — 화이트리스트에 없는 변수는 도달하지 않고,
    /// 시크릿은 도달한다.
    ///
    /// `/usr/bin/env`가 찍는 것은 자식이 **실제로 받은** 환경이므로, 이 테스트는 우리
    /// 구현이 아니라 커널이 자식에게 준 것을 본다.
    #[tokio::test]
    async fn child_env_is_whitelist_only() {
        let Some(env_bin) = observer("/usr/bin/env") else {
            return;
        };
        let marker_name = "XB_TEST_LEAK_MARKER";
        let marker_value = "this-must-not-reach-the-child";

        // 프로세스 환경을 만지는 구간만 공용 가드로 감싼다. 자식 env는 **spawn 시점에
        // 확정**되므로(exec가 환경을 복사한다) 종료를 기다리는 await까지 잠금을 들고 있을
        // 이유가 없다 — 오히려 들고 있으면 같은 가드를 쓰는 다른 테스트를 자식이 끝날
        // 때까지 세운다.
        let job = {
            let _guard = crate::web::job::env_guard();

            // 서버 프로세스에 표식을 심는다 — 화이트리스트에 없으므로 자식에 나타나면 안 된다.
            std::env::set_var(marker_name, marker_value);
            // config 오버라이드 모양의 변수도 심는다(가장 새기 쉬운 형태).
            std::env::set_var("XB_SOURCE__URI", "mongodb://leak:leak@host/db");
            std::env::set_var("XB_PROFILE", "leaked-profile");
            std::env::set_var("RUST_LOG", "trace");

            let mut secrets = JobSecrets::new();
            secrets.insert_value("XB_TEST_CHILD_SECRET", "child-only-secret-value");

            let runner = JobRunner::with_exe(env_bin, None, Lang::En, secrets);
            let job = runner.spawn_raw(Vec::new()).expect("spawn 실패");

            std::env::remove_var(marker_name);
            std::env::remove_var("XB_SOURCE__URI");
            std::env::remove_var("XB_PROFILE");
            std::env::remove_var("RUST_LOG");
            job
        };

        let child_env = job.wait_with_output().await.expect("wait 실패").stdout;
        // 1) 표식과 오버라이드성 변수는 하나도 새지 않는다.
        for leaked in [
            marker_value,
            "XB_TEST_LEAK_MARKER",
            "XB_SOURCE__URI",
            "XB_PROFILE",
            "leaked-profile",
            "RUST_LOG",
        ] {
            assert!(
                !child_env.contains(leaked),
                "자식 env에 '{leaked}'가 새어 나갔다:\n{child_env}"
            );
        }
        // 2) 자식 env의 모든 이름이 화이트리스트 ∪ 주입한 시크릿 이름에 속한다.
        for line in child_env.lines() {
            let Some((name, _)) = line.split_once('=') else {
                continue; // 여러 줄 값의 이어지는 줄.
            };
            assert!(
                FORWARDED_ENV_NAMES.contains(&name) || name == "XB_TEST_CHILD_SECRET",
                "화이트리스트에 없는 '{name}'이 자식 env에 있다"
            );
        }
        // 3) 시크릿은 그 자식에게 도달한다(주입이 실제로 동작한다).
        assert!(
            child_env.contains("XB_TEST_CHILD_SECRET=child-only-secret-value"),
            "시크릿이 자식에 주입되지 않았다:\n{child_env}"
        );
    }

    /// 시크릿은 서버 프로세스 환경에 상주하지 않는다 — `take_from_env`가 읽은 즉시
    /// 제거하고, 그래도 자식에는 도달한다.
    #[tokio::test]
    async fn secrets_do_not_remain_in_server_environment() {
        let Some(env_bin) = observer("/usr/bin/env") else {
            return;
        };
        let name = "XB_TEST_TAKEN_URI";
        let value = "mongodb://user:hunter2pass@db1/app";

        // env를 만지는 구간만 잠근다(위 테스트와 같은 이유 — 자식 env는 spawn 시점에 확정).
        let job = {
            let _guard = crate::web::job::env_guard();
            std::env::set_var(name, value);

            let mut secrets = JobSecrets::new();
            let taken = secrets.take_from_env(&[name.to_string()]);
            assert_eq!(taken, 1, "시크릿을 읽지 못했다");

            // 서버 환경에서 사라졌다.
            assert!(
                std::env::var(name).is_err(),
                "시크릿이 서버 프로세스 환경에 남아 있다"
            );
            // 마스킹 레지스트리에는 등록되어 있다(자식 출력에 원문이 섞여도 지운다).
            assert!(secrets
                .registry()
                .mask(&format!("auth failed for {value}"))
                .contains(crate::web::mask::REDACTED_PLACEHOLDER));

            let runner = JobRunner::with_exe(env_bin, None, Lang::En, secrets);
            runner.spawn_raw(Vec::new()).expect("spawn 실패")
        };

        let completion = job.wait_with_output().await.expect("wait 실패");
        assert!(
            completion.stdout.contains(&format!("{name}={value}")),
            "서버 환경에서 제거한 시크릿이 자식에 도달하지 않았다"
        );
    }

    // ---- 셸 경유 없음 ----

    /// 자식은 셸을 경유하지 않는다 — 셸 메타문자가 든 인자가 **하나의 인자 그대로**
    /// 전달되고, 아무것도 실행되지 않는다.
    ///
    /// 정상 경로에서는 [`JobSpec`]이 이런 값을 애초에 거부하므로(`args` 모듈), 여기서는
    /// 테스트 전용 `spawn_raw`로 검증 계층을 건너뛰고 **spawn 계층 자체의 성질**을 본다.
    #[tokio::test]
    async fn child_does_not_go_through_a_shell() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let hostile = vec![
            "a;whoami".to_string(),
            "$(whoami)".to_string(),
            "`whoami`".to_string(),
            "x&&id".to_string(),
            "y||id".to_string(),
            "*".to_string(),
            "$HOME".to_string(),
            "a b\tc".to_string(),
        ];

        let runner = JobRunner::with_exe(echo_bin, None, Lang::En, JobSecrets::new());
        let completion = runner
            .spawn_raw(hostile.clone())
            .expect("spawn 실패")
            .wait_with_output()
            .await
            .expect("wait 실패");

        let out = completion.stdout;
        // 각 인자가 문자 그대로 살아 있다 — 치환·확장·명령 실행이 없었다는 뜻이다.
        for arg in &hostile {
            assert!(
                out.contains(arg),
                "인자 {arg:?}가 그대로 전달되지 않았다: {out:?}"
            );
        }
        // `$HOME`이 확장되지 않았다(확장됐다면 셸을 지났다는 뜻).
        if let Ok(home) = std::env::var("HOME") {
            if !home.is_empty() {
                assert!(
                    !out.contains(&home),
                    "$HOME이 확장됐다 — 셸을 경유했다: {out:?}"
                );
            }
        }
        // `*`가 글롭 확장되지 않았다.
        assert!(out.contains('*'), "글롭이 확장됐다: {out:?}");
        // echo는 인자를 공백 하나로 이어 한 줄만 찍는다 — 인자가 명령으로 쪼개졌다면
        // 줄 수가 늘거나 내용이 달라진다.
        assert_eq!(
            out.lines().count(),
            1,
            "출력이 한 줄이 아니다(인자가 쪼개졌다): {out:?}"
        );
    }

    /// 전역 플래그는 서브커맨드 **앞**에, 잡 argv는 그 뒤에 놓인다.
    #[tokio::test]
    async fn global_flags_precede_the_subcommand() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let runner = JobRunner::with_exe(
            echo_bin,
            Some(PathBuf::from("/etc/x-backup/config.toml")),
            Lang::Ko,
            JobSecrets::new(),
        );
        let spec = JobSpec::new(JobCommand::Doctor, Lang::En);
        let completion = runner
            .spawn(&spec)
            .expect("spawn 실패")
            .wait_with_output()
            .await
            .expect("wait 실패");
        let out = completion.stdout.trim().to_string();
        assert_eq!(
            out, "--config /etc/x-backup/config.toml --lang ko doctor --json",
            "argv 조립이 달라졌다"
        );
    }

    /// `--lang` 값이 clap이 받는 어휘와 일치한다 — 값이 갈라지면 모든 잡이 exit 2다.
    #[test]
    fn lang_arg_round_trips_through_clap() {
        use clap::Parser;
        for lang in [Lang::En, Lang::Ko] {
            let cli =
                crate::cli::Cli::try_parse_from(["x-backup", "--lang", lang_arg(lang), "doctor"])
                    .unwrap_or_else(|e| panic!("--lang {}를 clap이 거부했다: {e}", lang_arg(lang)));
            assert_eq!(cli.lang, Some(lang));
        }
    }

    // ---- 파괴적 작업 게이트 ----

    /// 파괴적 명령은 `spawn`으로 실행할 수 없다 — 감사 게이트를 거쳐야 한다.
    #[test]
    fn spawn_refuses_destructive_commands() {
        let runner = JobRunner::new(None, Lang::En, JobSecrets::new()).expect("러너 생성 실패");
        for command in [JobCommand::Restore, JobCommand::Prune, JobCommand::Migrate] {
            let spec = JobSpec::new(command, Lang::En)
                .with_profile(ProfileName::parse("p", Lang::En).unwrap());
            let err = runner
                .spawn(&spec)
                .err()
                .unwrap_or_else(|| panic!("{}가 게이트 없이 실행됐다", command.verb()));
            assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
            assert!(
                err.to_string().contains("AuditReceipt"),
                "무엇이 필요한지 메시지에 있어야 함: {err}"
            );
        }
    }

    /// 비파괴 명령은 게이트 없이 실행된다(과다 게이팅으로 콘솔이 마비되지 않는다).
    #[tokio::test]
    async fn spawn_allows_non_destructive_commands() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let runner = JobRunner::with_exe(echo_bin, None, Lang::En, JobSecrets::new());
        for command in [
            JobCommand::Backup,
            JobCommand::Verify,
            JobCommand::Status,
            JobCommand::List,
            JobCommand::Peek,
            JobCommand::Doctor,
        ] {
            let spec = JobSpec::new(command, Lang::En);
            let job = runner
                .spawn(&spec)
                .unwrap_or_else(|e| panic!("{}가 거부됐다: {e}", command.verb()));
            let completion = job.wait_with_output().await.expect("wait 실패");
            assert!(completion.stdout.contains(command.verb()));
        }
    }

    /// 감사 기록(gate) → receipt → 파괴적 실행의 **순서**가 실제로 성립한다.
    ///
    /// [`AuditReceipt`]는 [`crate::web::audit`] 밖에서 만들 수 없고
    /// [`JobRunner::spawn_destructive`]는 그 값을 요구하므로, "기록 없이 복구를 실행하는"
    /// 호출은 컴파일되지 않는다(넘길 값을 만들 방법이 없다). 이 테스트는 그 성립 순서를
    /// 런타임으로 확인한다 — gate가 남긴 `requested` 줄이 실행 **전에** 파일에 있다.
    #[tokio::test]
    async fn destructive_spawn_requires_receipt_recorded_first() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let log = crate::web::audit::AuditLog::open(dir.path()).unwrap();
        let runner = JobRunner::with_exe(echo_bin, None, Lang::En, JobSecrets::new());

        let spec = JobSpec::new(JobCommand::Restore, Lang::En)
            .with_profile(ProfileName::parse("prod", Lang::En).unwrap())
            .with_flag(JobFlag::Force);

        // gate가 성공해야만 receipt가 생긴다.
        let receipt = log
            .gate(
                "operator",
                spec.audit_action(),
                spec.audit_target(),
                &spec.masked_args(runner.secret_registry()),
            )
            .await
            .expect("정상 경로에서 gate는 성공해야 함");

        // 실행 **전에** 이미 기록이 파일에 있다.
        let recorded = std::fs::read_to_string(log.path()).unwrap();
        assert!(
            recorded.contains("\"outcome\":\"requested\""),
            "실행 전에 requested 기록이 없다: {recorded}"
        );
        assert!(recorded.contains("restore.run"));
        assert!(recorded.contains("prod"));

        // receipt를 소비해 실행한다.
        let completion = runner
            .spawn_destructive(&spec, receipt)
            .expect("receipt가 있으면 실행되어야 함")
            .wait_with_output()
            .await
            .expect("wait 실패");
        assert!(completion.stdout.contains("restore"));

        // 완료 기록은 호출부의 몫이다 — 여기서 남기면 두 번 기록된다(t8 규약).
        let outcome = completion.outcome;
        log.record(crate::web::audit::AuditEvent {
            actor: "operator",
            action: spec.audit_action(),
            target: spec.audit_target(),
            args_masked: &spec.masked_args(runner.secret_registry()),
            outcome: outcome.audit_outcome(),
            exit_code: outcome.exit_code(),
        })
        .await
        .unwrap();
        let recorded = std::fs::read_to_string(log.path()).unwrap();
        let lines: Vec<&str> = recorded.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "요청 1줄 + 완료 1줄이어야 한다");
        assert!(lines[0].contains("requested"));
        assert!(lines[1].contains("success"));
    }

    /// gate가 실패하면 receipt가 없으므로 파괴적 실행으로 갈 경로가 없다.
    #[cfg(unix)]
    #[tokio::test]
    async fn failed_gate_yields_no_path_to_destructive_spawn() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let log = crate::web::audit::AuditLog::open(dir.path()).unwrap();
        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o400)).unwrap();

        let outcome = log.gate("operator", "restore.run", "prod", &[]).await;

        std::fs::set_permissions(log.path(), std::fs::Permissions::from_mode(0o600)).unwrap();

        // Err 분기에는 AuditReceipt 값이 없다 — spawn_destructive를 부를 방법이 없다.
        assert!(outcome.is_err(), "쓰기 불가 상태에서 gate가 성공하면 안 됨");
    }

    // ---- 실제 x-backup 자식 e2e ----

    /// 빌드된 `x-backup` 바이너리 경로 — 없으면 `None`(테스트를 건너뛴다).
    ///
    /// ## `assert_cmd::cargo::cargo_bin`을 쓰지 않는 이유 — 그 함수가 패닉한다
    /// `CARGO_BIN_EXE_<name>`은 카고가 **통합 테스트**(`tests/`)에만 넣어 주는 변수다.
    /// 라이브러리 단위 테스트에는 없고, `cargo_bin`은 그 변수가 없으면 경로를 돌려주는
    /// 대신 **패닉한다**. 그래서 "없으면 건너뛴다"는 이 함수가 오히려 테스트를 죽였다:
    /// `cargo test --lib`(CI가 도는 명령)에서 6개가 실패하고, `cargo test`로 전 타깃을
    /// 돌리면 bin이 함께 빌드되어 변수가 채워지므로 **통과한다** — 그래서 로컬에서는
    /// 보이지 않았다.
    ///
    /// 두 경로를 모두 본다: 변수가 있으면 그것(가장 정확하다), 없으면 테스트 바이너리
    /// 위치에서 프로파일 디렉터리를 거슬러 올라가 추정한다
    /// (`target/<profile>/deps/<test>-<hash>` → `target/<profile>/x-backup`).
    fn built_binary() -> Option<PathBuf> {
        let bin = match option_env!("CARGO_BIN_EXE_x-backup") {
            Some(path) => PathBuf::from(path),
            None => {
                let exe = std::env::current_exe().ok()?;
                // deps/<test-bin> → deps → <profile>
                let profile_dir = exe.parent()?.parent()?;
                profile_dir.join("x-backup")
            }
        };
        if bin.exists() {
            Some(bin)
        } else {
            eprintln!(
                "건너뜀: {} 바이너리가 아직 빌드되지 않았습니다",
                bin.display()
            );
            None
        }
    }

    /// 실제 `x-backup doctor --json` 자식을 띄워 spawn→종료 코드 분류까지 확인한다.
    ///
    /// `doctor`를 고른 이유: DB·네트워크 없이 오프라인으로 돌고, config만 있으면
    /// 결정적으로 0/3/4 중 하나를 낸다.
    ///
    /// 바이너리가 아직 빌드되지 않았으면(`cargo test --lib`만 돌린 경우) 건너뛴다 —
    /// 라이브러리 테스트가 bin 타깃 빌드 여부에 묶이지 않게 한다.
    #[tokio::test]
    async fn real_child_doctor_runs_and_classifies_exit_code() {
        let Some(bin) = built_binary() else {
            return;
        };

        // 최소 유효 config(tests/exit_codes_e2e.rs의 패턴). destination은 local tempdir.
        let dest = tempfile::tempdir().unwrap();
        let mut cfg = tempfile::Builder::new()
            .prefix("x-backup-job-runner-")
            .suffix(".toml")
            .tempfile()
            .unwrap();
        use std::io::Write;
        write!(
            cfg,
            r#"
default_profile = "p"

[profiles.p.mode]
backup_type = "full"
output      = "quiet"
precheck    = true

[profiles.p.source]
uri_env = "XB_JOB_RUNNER_URI_UNUSED"

[profiles.p.destination]
type = "local"
path = "{}"

[profiles.p.features.compression]
algorithm = "zstd"
level     = 3

[profiles.p.features.encryption]
enabled = false
"#,
            dest.path().to_str().unwrap()
        )
        .unwrap();
        cfg.flush().unwrap();

        let runner = JobRunner::with_exe(
            bin,
            Some(cfg.path().to_path_buf()),
            Lang::En,
            JobSecrets::new(),
        );
        let spec = JobSpec::new(JobCommand::Doctor, Lang::En);
        let job = runner.spawn(&spec).expect("doctor 자식 spawn 실패");

        // 핸들 정보(t12가 쓸 것)가 채워져 있다.
        assert!(job.pid().is_some(), "pid가 없다");
        assert_eq!(job.handle().command, "doctor");
        let started = job.started_at();

        let completion = job.wait_with_output().await.expect("wait 실패");

        // doctor는 0(정상)/4(경고)/3(차단성 설정 오류) 중 하나다 — 시그널·미상·사용법
        // 오류가 나오면 argv 조립이나 실행 경로가 잘못됐다는 뜻이다.
        assert!(
            matches!(
                completion.outcome,
                JobOutcome::Succeeded
                    | JobOutcome::SucceededWithWarnings
                    | JobOutcome::PrecheckFailed
            ),
            "doctor 종료 상태가 예상 밖이다: {:?}\nstdout: {}\nstderr: {}",
            completion.outcome,
            completion.stdout,
            completion.stderr
        );
        assert!(
            completion.outcome.exit_code().is_some(),
            "정상 종료인데 종료 코드가 없다"
        );

        // `--json`이 실제로 붙었다 — stdout이 JSON 문서로 파싱된다.
        let parsed: serde_json::Value = serde_json::from_str(completion.stdout.trim())
            .unwrap_or_else(|e| {
                panic!(
                    "doctor stdout이 JSON이 아니다({e}) — --json이 붙지 않았을 수 있다:\n{}",
                    completion.stdout
                )
            });
        assert!(
            parsed.get("schema").is_some(),
            "doctor JSON에 schema 필드가 없다: {parsed}"
        );

        // 시작 시각은 부모가 spawn 직전에 찍은 값이므로 현재보다 과거다(t12가 pid 재사용을
        // 걸러낼 때 쓰는 하한 — [`JobHandle::started_at`]).
        assert!(started <= Utc::now(), "시작 시각이 미래다");
    }

    /// 존재하지 않는 바이너리를 가리키면 spawn이 명확한 실패로 끊긴다(조용히 성공하지
    /// 않는다).
    #[test]
    fn spawn_failure_is_reported_with_path_and_command() {
        let runner = JobRunner::with_exe(
            PathBuf::from("/nonexistent/x-backup-does-not-exist"),
            None,
            Lang::En,
            JobSecrets::new(),
        );
        let err = runner
            .spawn(&JobSpec::new(JobCommand::Doctor, Lang::En))
            .expect_err("없는 바이너리로 spawn이 성공하면 안 됨");
        let msg = err.to_string();
        assert!(msg.contains("x-backup-does-not-exist"), "{msg}");
        assert!(msg.contains("doctor"), "{msg}");
    }

    /// `wait` 이후 핸들의 pid가 비워진다 — 수거된 pid는 재사용될 수 있으므로 t12가
    /// 그 값에 시그널을 보내면 남의 프로세스를 죽인다.
    #[tokio::test]
    async fn pid_is_cleared_after_wait() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let runner = JobRunner::with_exe(echo_bin, None, Lang::En, JobSecrets::new());
        let mut job = runner
            .spawn(&JobSpec::new(JobCommand::List, Lang::En))
            .unwrap();
        assert!(job.pid().is_some());
        let outcome = job.wait().await.expect("wait 실패");
        assert_eq!(outcome, JobOutcome::Succeeded, "/bin/echo는 0으로 끝난다");
        assert!(job.pid().is_none(), "수거 후에도 pid가 남아 있다");
    }

    /// t11이 쓸 스트림 핸들이 실제로 꺼내지고, 두 번째 호출은 `None`이다(파이프의
    /// 주인은 하나뿐).
    #[tokio::test]
    async fn streams_are_exposed_once_for_the_relay_layer() {
        let Some(echo_bin) = observer("/bin/echo") else {
            return;
        };
        let runner = JobRunner::with_exe(echo_bin, None, Lang::En, JobSecrets::new());
        let mut job = runner
            .spawn(&JobSpec::new(JobCommand::Status, Lang::En))
            .unwrap();

        let stdout = job.take_stdout();
        assert!(stdout.is_some(), "t11이 쓸 stdout 핸들이 없다");
        assert!(job.take_stdout().is_none(), "stdout이 두 번 꺼내졌다");
        assert!(job.take_stderr().is_some(), "stderr 핸들이 없다");
        assert!(job.take_stderr().is_none(), "stderr이 두 번 꺼내졌다");

        // 꺼낸 스트림을 끝까지 읽어야 자식이 막히지 않는다(모듈 헤더의 교착 주의).
        use tokio::io::AsyncReadExt;
        let mut buf = String::new();
        stdout.unwrap().read_to_string(&mut buf).await.unwrap();
        assert!(buf.contains("status"));
        assert!(job.wait().await.unwrap().is_success());
    }

    /// 러너의 `Debug`는 env 값과 시크릿을 노출하지 않는다.
    #[test]
    fn runner_debug_hides_env_values_and_secrets() {
        let _guard = crate::web::job::env_guard();
        std::env::set_var("TZ", "Asia/Seoul");
        let mut secrets = JobSecrets::new();
        secrets.insert_value("XB_TEST_DEBUG_SECRET", "do-not-print-this-value");

        let runner = JobRunner::with_exe(PathBuf::from("/bin/true"), None, Lang::En, secrets);
        let rendered = format!("{runner:?}");
        std::env::remove_var("TZ");

        assert!(
            !rendered.contains("do-not-print-this-value"),
            "시크릿 값이 Debug로 새어 나갔다: {rendered}"
        );
        assert!(
            !rendered.contains("Asia/Seoul"),
            "env 값이 Debug로 새어 나갔다: {rendered}"
        );
        // 이름은 진단에 필요하므로 보인다.
        assert!(rendered.contains("TZ"), "전달 env 이름이 없다: {rendered}");
    }

    /// 빈 값 env는 전달하지 않는다("설정됨"과 "빈 값"을 구분한다).
    #[test]
    fn empty_env_values_are_not_forwarded() {
        let _guard = crate::web::job::env_guard();
        std::env::set_var("TZ", "");
        let snapshot = snapshot_forwarded_env();
        std::env::remove_var("TZ");
        assert!(
            !snapshot.iter().any(|(k, _)| k == "TZ"),
            "빈 값 env가 전달됐다"
        );
    }
}
