//! 잡 생명주기 — 취소(SIGTERM→SIGKILL 승격)와 서버 재시작 후 고아 잡 재부착.
//!
//! [`runner`](super::runner)는 자식을 **띄우는** 계층이고, 이 파일은 이미 떠 있는(또는
//! 떠 있었던) 자식을 **다루는** 계층이다. 백업/복구는 수십 분~시간 단위로 돌므로 두 가지가
//! 필요하다: 운영자가 잘못 띄운 잡을 멈출 수 있어야 하고(취소), 콘솔 자체가 재시작돼도
//! 진행 중인 백업이 죽지 않아야 한다(재부착). 이 파일은 그 둘을 모두 **pid에 직접
//! 시그널을 보내는 방식**으로 구현한다 — [`super::runner::RunningJob`]은 `Child`를
//! 사사롭게 감싸고 있어 임의 시그널(SIGTERM)을 보낼 API를 노출하지 않기 때문이다
//! (tokio의 `Child::kill()`은 항상 SIGKILL이다). 그래서 여기서는 `libc::kill`을 직접
//! 쓴다 — [`crate::lock::file_lock`]이 `pid_alive` 판정에 이미 쓰고 있는 것과 같은
//! 원시 도구다.
//!
//! ## 최악의 실패는 "남의 프로세스를 죽이는 것"이다
//! pid는 재사용된다. 잡 A가 pid 4242로 끝나고, 운영자가 화면을 새로고침하기 전에
//! 완전히 무관한 프로세스 B가 우연히 같은 pid 4242를 받으면, "A를 취소해줘"라는 요청이
//! B를 죽인다. 이걸 막는 유일한 방법은 **시그널을 보내기 전에 그 pid가 정말 우리가
//! 기록한 그 프로세스인지 독립적으로 재확인하는 것**이다. 이 파일의 거의 모든 코드는
//! 그 재확인([`verify_identity`])을 중심으로 조직되어 있다:
//!
//! 1. **생존**([`pid_alive`]) — `kill(pid, 0)`(POSIX, 시그널을 실제로 보내지 않고
//!    존재/권한만 검사한다). 죽었으면 그 자리에서 끝(신호를 보낼 필요도 대상도 없다).
//! 2. **시작 시각**([`os_process_start_time`]) — OS가 **독립적으로** 보고하는 그 pid의
//!    실제 시작 시각을, 우리가 spawn 직후 기록해 둔 값과 비교한다. 우리가 스스로 기록한
//!    값과 우리가 스스로 기록한 값을 비교하면 아무것도 검증하지 못한다(같은 소스가
//!    틀렸으면 둘 다 틀린다) — 그래서 반드시 OS에 **다시 물어야** 한다.
//! 3. **명령행**([`os_process_cmdline_contains`]) — 그 pid가 실제로 우리 프로파일 이름을
//!    인자로 들고 있는지까지 본다.
//!
//! 세 축 중 하나라도 확인할 수 없으면(예: `ps`도 `/proc`도 없는 환경) **거부한다**
//! (fail-closed). "취소가 가끔 안 먹는다"는 답답하지만 복구 가능한 실패이고, "엉뚱한
//! 프로세스가 죽는다"는 되돌릴 수 없는 실패다 — 이 파일의 모든 판단은 후자를 피하는
//! 쪽으로 기운다.
//!
//! ### 취소도 3축을 요구한다 — 이 헤더가 한때 2축이면 충분하다고 적었던 이유와, 그게 왜
//! ### 틀렸는지
//! 처음 이 파일은 "취소는 **자신이 방금 띄운 자식**을 대상으로 하므로 1·2축만으로 충분하고,
//! 재부착만 디스크 기록으로 판단하니 3축이 필요하다"고 적어 두고 [`cancel`]을 2축으로
//! 구현했다. 그 전제가 실제 호출부와 맞지 않았다: `POST /backup/{id}/cancel`
//! ([`crate::web::routes::backup::cancel`])은 **영속된 인덱스에서** pid·시작 시각을 읽어
//! 넘긴다([`crate::web::state::jobs::JobStore::detail`]). "이 프로세스가 방금 띄운
//! 자식인가"를 확인하는 코드는 어디에도 없었고, 서버가 재시작한 뒤에도 같은 경로로 취소가
//! 도달한다 — 즉 취소도 **디스크 기록만 보고 판단**하고 있었다. 그 상태에서 pid가 회수·재사용되고
//! 새 프로세스의 시작 시각이 우연히 기록값 ±[`START_TIME_TOLERANCE_MS`] 안에 들어오면
//! `kill(-pid, SIGTERM)`이 **남의 프로세스 그룹 전체**로 간다.
//!
//! 확률은 매우 낮다(pid_max가 한 바퀴 돌아야 한다). 고친 이유는 확률이 아니라 **근거가
//! 경로를 덮지 못했다**는 것이다: 안전 논증이 "우리 자식이니까"에 기대고 있었는데 실제
//! 경로에는 그 전제가 없었다. 그래서 [`verify_identity`] 하나가 세 축을 모두 보고,
//! [`cancel`]과 [`classify_for_reattach`]가 **같은 판정 함수**를 쓴다 — 두 경로가 같은
//! 종류의 증거(디스크에 남은 기록)를 쓰므로 같은 기준을 적용하는 것이 맞다. 비용은
//! `/proc/<pid>/cmdline` 읽기 한 번(Linux) 또는 `ps` 한 번(macOS)이고, 취소는 사람이
//! 버튼을 누를 때만 일어나므로 무의미한 수준이다.
//!
//! 대가도 정직하게 밝힌다: 프로파일이 argv에 없는 잡은 이제 취소할 수 없다. 실제로는
//! 제약이 아니다 — [`JobIdentity::profile`]이 필수이고 [`JobRecord::from_handle`]은 프로파일
//! 없는 잡(`list`/`doctor`)에 아예 `None`을 주므로, 취소 대상이 되는 잡(`backup`/`restore`/
//! `prune`)은 항상 `--profile <name>`을 argv에 들고 있다.
//!
//! ## 왜 SIGTERM을 먼저 보내는가 — 그리고 지금 이 전제가 부분적으로만 성립한다는 것
//! `SIGKILL`은 커널이 그 자리에서 프로세스를 지운다 — 사용자 코드가 한 줄도 더 돌지
//! 않으므로 [`crate::lock::LockGuard`]의 `Drop`이 실행되지 않고, lock 파일이 디스크에
//! 남는다. `SIGTERM`은 반대로 **프로세스가 직접 처리(또는 기본 동작으로 종료)할 기회를
//! 준다** — 자식이 SIGTERM을 정상 반환 경로로 받으면 `LockGuard`가 스택에서 빠지며
//! Drop이 돌고 lock 파일이 즉시 사라진다. 그래서 이 파일은 항상 SIGTERM을 먼저 쓰고,
//! grace 동안 반응이 없을 때만 SIGKILL로 승격한다([`cancel`]).
//!
//! **정직하게 밝힌다:** 이 근거는 **자식이 SIGTERM에 커스텀 핸들러를 설치했을 때만**
//! 완전하다. 이 크레이트를 훑어보면 [`crate::web::server`]는 `serve` 자신의 graceful
//! shutdown을 위해 `tokio::signal::unix::signal(SignalKind::terminate())`를 설치하지만,
//! `backup`/`restore`/`prune`/`migrate` 핸들러(`src/cli/handlers/*.rs`)는 **SIGTERM
//! 핸들러를 설치하지 않는다.** 즉 지금은 SIGTERM의 기본 동작(SIG_DFL = 즉시 종료, 정리
//! 코드 없음)이 적용되어 SIGTERM만으로도 lock 파일이 디스크에 남을 수 있다. 이건 이
//! 파일(t12, `job/lifecycle.rs`)의 범위 밖이다 — 고치려면 `src/cli/handlers/backup.rs`
//! 등에 `tokio::signal::unix::signal(SignalKind::terminate())`를 설치하고 정상 반환
//! 경로로 빠지게 해야 하는데, 그건 이 태스크가 손댈 수 있는 파일이 아니다. **다행히
//! 안전망은 이미 있다:** [`crate::lock::file_lock`]의 stale lock 회수 정책이 "pid 부재"를
//! 자동 감지해 **다음 acquire 시도에서** 죽은 소유자의 lock을 회수한다(파일 헤더의
//! 정책 1). 그래서 SIGTERM 직후 lock 파일이 즉시 사라지지 않더라도, 같은 프로파일을
//! 다시 백업하려는 다음 시도가 스스로 정리한다 — 사용자에게 보이는 결과("취소했는데
//! 왜 lock이 아직 있지?")는 조금 어색하지만 안전(이중 실행 없음)은 깨지지 않는다.
//! 아래 테스트(`sigterm_stops_a_lock_holder_and_removes_the_lock_file`)는 **SIGTERM을
//! 실제로 받아 처리하는(=핸들러가 있는) 자식**을 대상으로 이 파일의 취소 로직 자체가
//! 옳게 동작함을 증명한다 — CLI 핸들러들이 나중에 그 계약을 채우면 이 파일은 코드
//! 변경 없이 곧바로 혜택을 본다.
//!
//! 그럼에도 SIGKILL을 첫 수단으로 쓰지 않는 이유는 두 가지 더 있다: (1) 손자
//! 프로세스(`mongodump`/`pg_dump`)는 자신만의 SIGTERM 처리를 갖고 있을 수 있다 — 이건
//! 우리 통제 밖이고, SIGTERM을 주면 그 도구가 스스로 정리할 기회라도 준다. (2) 위
//! 한계가 CLI 핸들러 쪽에서 채워지는 순간(누군가 SIGTERM 핸들러를 추가하는 순간) 이
//! 파일은 아무것도 바꾸지 않아도 자동으로 완전해진다 — SIGKILL을 기본값으로 박아두면
//! 그 개선이 영영 쓰일 일이 없다.
//!
//! ## grace 타임아웃과 SIGKILL 승격
//! [`CANCEL_GRACE_DEFAULT`](30초)만큼 기다려도 살아 있으면 [`send_group_signal`]로
//! SIGKILL을 보낸다. 근거와 상한은 그 상수의 doc 참조. 승격 전 재검증은 하지 않는다 —
//! grace 내내 [`CANCEL_POLL_INTERVAL`]마다 생존을 계속 관찰했으므로, 그 사이 pid가
//! 회수되고 재사용될 틈이 없었다(죽지 않은 프로세스의 pid는 재사용될 수 없다).
//!
//! **다만 "SIGKILL을 보냈다"와 "죽었다"는 다르다.** 커널이 전송 자체를 거부할 수 있고
//! (`EPERM` — 다른 사용자가 띄운 잡, 샌드박스 정책), 그때 대상이 살아 있으면 취소는
//! 이뤄지지 않은 것이다. 예전 코드는 `EPERM`을 무조건 성공으로 접어 그 경우에도
//! [`CancelOutcome::KilledAfterGrace`]를 반환했다 — **취소 실패를 성공으로 보고한 것이다.**
//! 지금은 [`interpret_eperm`]이 [`pid_finished`]로 조건을 실제로 확인하고, 살아 있으면
//! [`CancelOutcome::Refused`]로 접는다(그 함수 doc 참조).
//!
//! ## SIGKILL은 프로세스 그룹 전체로 간다
//! [`runner`](super::runner)가 자식을 `process_group(0)`으로 띄워 자기 그룹의
//! 리더로 만들어 둔다(그 파일 헤더 참조). 이 파일은 항상 `kill(-pid, sig)`(음수 pid =
//! 그룹 전체)을 쓴다 — 직계 자식만 죽이면 `mongodump`/`pg_dump` 같은 손자가 고아로
//! 남아 계속 돈다. 서버가 재시작해도 프로세스 그룹 소속은 그대로 유지되므로(그룹은
//! 부모-자식 관계가 아니라 프로세스 자신의 속성이다), 재부착된 고아 잡에도 같은
//! `-pid` 대상이 그대로 유효하다.
//!
//! ## 시작 시각을 어떻게 얻는가 — 플랫폼 한계
//! [`os_process_start_time`]은 두 갈래다:
//!
//! - **Linux:** `/proc/<pid>/stat`의 22번째 필드(부팅 이후 클럭 틱)를 `/proc/stat`의
//!   `btime`(부팅 시각, 초 단위)과 합쳐 절대 UTC 시각을 만든다. 외부 프로세스를 띄우지
//!   않고 파일 두 개만 읽으므로 빠르고 신뢰할 수 있다. `/proc`가 마운트되지 않은 매우
//!   드문 환경(제한된 chroot·일부 컨테이너 보안 프로파일)에서는 실패해 `None`을 준다 —
//!   그 경우 [`verify_identity`]는 fail-closed로 거부한다.
//! - **그 외 unix(주로 macOS):** `/proc`가 없으므로 `ps -p <pid> -o lstart=`를 **셸 없이**
//!   서브프로세스로 띄워 파싱한다(`Command::new("ps")` — `sh -c`가 아니다). 두 가지
//!   한계가 있다: (1) 초 단위로만 반올림된 값이라 [`START_TIME_TOLERANCE_MS`]로 흡수한다.
//!   (2) `lstart`는 **로컬 시각**이다(UTC가 아니다) — `chrono::Local`로 UTC 변환하는데,
//!   그 변환이 DST 전환이 벌어지는 바로 그 지역 시각(1년에 두 번, 각 한두 시간)과 겹치면
//!   모호하거나 존재하지 않는 로컬 시각이 되어 변환이 실패할 수 있다(`None` → 거부).
//!   그 창은 1년에 몇 시간뿐이고, 실패는 "취소가 막힌다"는 안전한 방향이므로 받아들인다.
//!
//! [`os_process_cmdline_contains`](재부착 전용)도 같은 구조다 — Linux는
//! `/proc/<pid>/cmdline`(NUL로 구분된 진짜 argv)을, 그 외에는 `ps -o args=`(폭 무제한
//! `-ww`)를 쓴다.
//!
//! ## t14와의 이음매 — [`JobIdentity`]
//! 이 파일은 잡 상태를 어디에 어떻게 영속화하는지 **모른다**(그건 t14
//! `src/web/state/jobs.rs`의 책임이고, 웹 계층 안에서도 모듈 간 결합을 최소화하려는
//! 선택이다). 대신 재부착·취소 판정에 필요한 최소 정보 세 개(pid·시작 시각·프로파일)를
//! [`JobIdentity`] trait 하나로 추상화해 노출한다. t14/리더는 자신의 영속 레코드
//! 타입(디스크에서 역직렬화한 구조체)에 이 trait을 구현하기만 하면 [`cancel`]과
//! [`classify_for_reattach`]를 그대로 쓸 수 있다. **살아 있는(아직 리다이렉트 전인)
//! 잡**은 [`super::runner::JobHandle`]을 이미 들고 있을 것이므로, 편의를 위해
//! [`JobRecord::from_handle`]로 바로 변환하는 경로도 열어 둔다.
//!
//! ## 재부착 — 진행률을 잃는다
//! 서버가 재시작되면 stdout 파이프(SSE 중계의 원천 — t11)는 완전히 사라진다. 이미 떠난
//! 프로세스와 새 프로세스 사이에 파이프를 다시 만들 방법은 없다(파이프는 `fork`할 때만
//! 만들어진다). 그래서 재부착된 잡은 [`REATTACHED_PROGRESS_LABEL`]로 "진행률 미상"임을
//! 밝혀야 하고, 완료 감지는 [`wait_for_orphan_exit`]의 폴링으로 대체한다(간격과 근거는
//! 그 함수 doc 참조).
//!
//! ## 이 파일이 하지 않는 것
//! - **lock을 잡거나 읽지 않는다.** 취소·재부착 판정은 pid·시작 시각·명령행만으로
//!   끝난다 — lock 파일은 자식이 관리하는 자원이고, 이 파일이 손대면 "자식이 잡는다"는
//!   불변식이 깨진다.
//! - **`crate::engine`·`crate::storage`·`crypto`를 부르지 않는다**([`crate::web`] 모듈
//!   헤더의 최상위 불변식).
//! - **`RunningJob::wait`를 대신 부르지 않는다.** SIGTERM/SIGKILL을 보내는 것과 종료
//!   상태를 수거하는 것은 다른 일이다([`super::runner`] 헤더 참조) — 수거는 호출부(t11의
//!   스트리밍 태스크이거나, 재부착된 잡이면 [`wait_for_orphan_exit`]을 부른 쪽)의 몫이다.

use std::io;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::error::{Result, XBackupError};

// ---------------------------------------------------------------------------
// 상수 — 매직 넘버 없이, 근거를 doc에 남긴다
// ---------------------------------------------------------------------------

/// 취소 grace 타임아웃 기본값 — SIGTERM 전송 후 SIGKILL로 승격하기까지 기다리는 시간.
///
/// ## 30초의 근거
/// - SIGTERM을 정상 처리하는 프로세스(파일 flush·소켓 정리·lock 해제)는 보통 수백
///   ms~수 초 안에 끝난다. `mongodump`/`pg_dump` 같은 손자 프로세스가 큰 출력 버퍼를
///   flush하는 극단적인 경우를 고려해도 30초는 충분히 넉넉하다.
/// - 상한이 없으면(SIGKILL을 아예 안 쓰면) 이 태스크의 존재 이유("운영자가 잘못 띄운
///   잡을 취소")가 무의미해진다 — 취소는 사람이 화면 앞에서 기다릴 수 있는 시간 안에
///   반응해야 쓸모가 있다.
/// - [`crate::web::routes::doctor::DOCTOR_TIMEOUT`](20초)와 같은 계열의 판단이다:
///   관측되는 정상 범위의 여러 배를 상한으로 잡되, 사람이 기다릴 수 있는 인간 척도를
///   벗어나지 않는다.
///
/// 호출부가 다른 값을 원하면 [`cancel`]에 직접 넘기면 된다 — 이 상수는 기본값일 뿐,
/// [`cancel`]의 시그니처를 강제하지 않는다.
pub const CANCEL_GRACE_DEFAULT: Duration = Duration::from_secs(30);

/// grace 대기 중 생존을 다시 확인하는 간격.
///
/// `kill(pid, 0)` 한 번은 사실상 공짜(문맥 전환이 없는 시스템 콜)이므로 짧게 잡아도
/// 비용이 없다. 200ms는 "취소가 사람 눈에 즉각적으로 느껴지는" 상한이면서, 30초 grace
/// 안에 최대 150번 폴링해도 부담이 없는 수준이다.
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// SIGKILL 전송 후 실제로 죽었는지 확인차 기다리는 짧은 상한.
///
/// SIGKILL은 가로챌 수 없으므로 정상 상태의 프로세스는 즉시 죽는다. 그래도 D-state(커널
/// 안에서 인터럽트 불가능한 대기 — 예: 먹통이 된 NFS I/O)에 걸린 프로세스는 SIGKILL조차
/// 즉시 듣지 않을 수 있다. 그런 경우까지 붙잡고 있을 이유는 없으므로 짧게만 확인하고,
/// 결과와 무관하게 [`CancelOutcome::KilledAfterGrace`]로 보고한다 — 우리가 할 수 있는
/// 조치는 이미 다 했고, 그 이상은 커널의 영역이다.
const KILL_CONFIRM_TIMEOUT: Duration = Duration::from_secs(2);

/// 재부착된 고아 잡의 종료를 감지하는 폴링 간격.
///
/// 재부착된 잡은 stdout 파이프를 잃어(모듈 헤더 "재부착 — 진행률을 잃는다") 종료를 알
/// 방법이 `kill(pid, 0)` 폴링뿐이다. 5초를 고른 이유:
///
/// - 백업은 수십 분~시간 단위로 돈다 — [`CANCEL_POLL_INTERVAL`](200ms)처럼 짧게 잡을
///   이유가 없다. 5초 지연은 "방금 끝난 몇 시간짜리 작업"의 화면 반영 지연으로는 무시할
///   수 있는 수준이다.
/// - `kill(pid, 0)` 자체는 사실상 공짜이므로, 이 간격은 "낭비를 줄이는" 튜닝이 아니라
///   "굳이 더 자주 깨어날 이유가 없다"는 판단이다 — 몇 시간짜리 폴링 태스크가 200ms마다
///   깨어나는 것도 무해하지만, 로그·디버깅 관점에서 조용한 편이 낫다.
pub const REATTACH_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// OS가 보고하는 프로세스 시작 시각과 우리가 기록한 시작 시각의 허용 오차(밀리초).
///
/// 두 값은 서로 다른 시계에서 나온다 — 우리 값은 spawn 직전에 우리가 직접 찍은
/// [`chrono::Utc::now()`]([`super::runner::JobHandle::started_at`]), OS 값은 플랫폼마다
/// 정밀도가 다르다(Linux `/proc`는 클럭 틱 단위로 서브초, macOS `ps -o lstart=`는 **초
/// 단위로 반올림**된다 — [`os_process_start_time`] 참조). 2000ms는 macOS의 반올림 오차
/// (최대 1초) + spawn 호출부터 커널이 실제로 exec을 마칠 때까지의 스케줄링 지연을 함께
/// 흡수하는 여유값이다. 이보다 더 벌어지면 "우연히 비슷한 시각에 뜬 다른 프로세스"로
/// 보수적으로 거부한다.
const START_TIME_TOLERANCE_MS: i64 = 2_000;

/// 재부착된 잡의 진행률 표시 — 화면·API가 그대로 쓸 수 있는 라벨(영문 고정,
/// [`crate::i18n`] 규약 — 라벨은 항상 영문).
pub const REATTACHED_PROGRESS_LABEL: &str = "reattached (progress unknown)";

// ---------------------------------------------------------------------------
// JobIdentity — t14와의 이음매
// ---------------------------------------------------------------------------

/// 취소·재부착 판정에 필요한 최소 식별 정보.
///
/// 이 파일은 잡 상태가 디스크에 어떻게 영속화되는지 모른다(그 스키마는 t14
/// `src/web/state/jobs.rs`의 책임). 대신 이 trait만 노출한다 — t14/리더는 자신의 영속
/// 레코드 타입(JSON에서 역직렬화한 구조체)에 이 trait을 구현하기만 하면
/// [`cancel`]·[`classify_for_reattach`]를 그대로 쓸 수 있다. 이 파일 밖 어디서 구현해도
/// 되도록 일부러 아주 작게 유지했다(pid·시각·프로파일 세 개뿐).
pub trait JobIdentity {
    /// 자식 pid.
    fn pid(&self) -> u32;
    /// 우리가 spawn 직후 기록한 시작 시각(UTC) — OS가 독립적으로 보고하는 실제 시작
    /// 시각과 비교해 pid 재사용을 가려낸다(모듈 헤더 참조).
    fn started_at(&self) -> DateTime<Utc>;
    /// 대상 프로파일 이름. "3중 일치"의 세 번째 축(명령행에 이 문자열이 인자로 있는지
    /// 확인한다 — [`os_process_cmdline_contains`]). 취소·재부착 **둘 다** 이 축을 요구한다
    /// (모듈 헤더 "취소도 3축을 요구한다").
    fn profile(&self) -> &str;
}

/// [`JobIdentity`]의 소유(owned) 구현 — 테스트와 값 전달이 간단한 간이 형태.
///
/// t14가 자신의 영속 레코드 타입에 직접 [`JobIdentity`]를 구현해도 되고, 편의상 이
/// 타입으로 변환해 써도 된다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    /// 자식 pid.
    pub pid: u32,
    /// 기록된 시작 시각(UTC).
    pub started_at: DateTime<Utc>,
    /// 대상 프로파일 이름.
    pub profile: String,
}

impl JobIdentity for JobRecord {
    fn pid(&self) -> u32 {
        self.pid
    }
    fn started_at(&self) -> DateTime<Utc> {
        self.started_at
    }
    fn profile(&self) -> &str {
        &self.profile
    }
}

impl JobRecord {
    /// 아직 살아 있는(같은 서버 프로세스 안에서 spawn된) [`super::runner::JobHandle`]에서
    /// 만든다.
    ///
    /// `pid`가 `None`(이미 [`super::runner::RunningJob::wait`]로 수거됨)이거나
    /// `profile`이 `None`(프로파일이 없는 명령 — `list`/`doctor` 등)이면 취소·재부착
    /// 판정의 대상이 아니므로 `None`을 준다. 취소·재부착은 실질적으로 프로파일이 있는
    /// 장기 실행 명령(backup/restore/prune)에만 의미가 있다.
    pub fn from_handle(handle: &super::runner::JobHandle) -> Option<Self> {
        Some(Self {
            pid: handle.pid?,
            started_at: handle.started_at,
            profile: handle.profile.clone()?,
        })
    }
}

// ---------------------------------------------------------------------------
// OS 원시 도구 — 생존·시작 시각·명령행 (플랫폼 분기는 여기 한 곳에만 있다)
// ---------------------------------------------------------------------------

/// `kill(pid, 0)`로 프로세스 생존을 확인한다.
///
/// [`crate::lock::file_lock`]의 같은 이름 함수와 로직이 동일하다 — 그 파일은 이 태스크가
/// 손댈 수 없고(팀 지시: "파일 락을 재구현하지 마라"), 그 함수도 `pub`이 아니라 이
/// 모듈에서 재사용(import)할 방법이 없다. 락 자체를 재구현하는 것과 "프로세스가
/// 살아있는가"라는 범용 OS 질문을 다시 묻는 것은 다르다 — 후자는 표준 POSIX 관용구
/// 10줄이고, 두 파일이 각자의 문맥(락 정책 vs 시그널 정책)에서 각자 갖는 편이 서로
/// 모르는 채로도 안전하다.
fn pid_alive(pid: u32) -> bool {
    // pid 0은 "호출자 프로세스 그룹 전체"를 뜻해 오판 위험이 있다 — 살아있는 것으로 본다
    // (lock 모듈과 동일한 보수적 판단).
    if pid == 0 {
        return true;
    }
    // SAFETY: kill(pid, 0)은 시그널을 보내지 않고 존재/권한만 검사한다(POSIX) — 메모리에
    // 영향이 없다.
    let ret = unsafe { libc::kill(pid as libc::pid_t, 0) };
    if ret == 0 {
        return true;
    }
    // ESRCH(없음)일 때만 죽음으로 판단한다. EPERM(권한 없음)은 존재함을 의미한다.
    io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// `siginfo_t`에서 `si_pid`를 읽는다 — 플랫폼마다 표현이 다르다(Linux는 유니온이라
/// `unsafe fn si_pid()` 접근자를, macOS/BSD는 평범한 `pub` 필드를 쓴다). [`waitid_peek`]가
/// 성공(`ret == 0`)했을 때만 호출한다 — 그 시점엔 커널이 이미 구조체를 채운 뒤다.
#[cfg(target_os = "linux")]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    // SAFETY: 방금 waitid(2)가 성공적으로 채운 구조체를 읽는다.
    unsafe { info.si_pid() }
}

#[cfg(not(target_os = "linux"))]
fn siginfo_pid(info: &libc::siginfo_t) -> libc::pid_t {
    info.si_pid
}

/// pid가 **우리 직계 자식**이라면, 회수(reap)하지 않고 종료 여부만 엿본다.
///
/// ## 왜 `kill(pid, 0)`만으로는 부족한가 — 좀비 문제
/// 자식이 SIGTERM을 받아 죽어도, 누군가 `wait()`로 회수하기 전까지는 좀비(zombie)로
/// 커널 프로세스 테이블에 남는다. **좀비도 `kill(pid, 0)`엔 성공으로 응답한다**(존재
/// 자체는 여전히 참이므로) — 그래서 [`pid_alive`]만 쓰면 "SIGTERM으로 이미 죽은 자식"과
/// "여전히 살아 있는 자식"을 구분하지 못하고, [`cancel`]이 매번 grace 타임아웃을 다
/// 채운 뒤에야(그리고 이 크레이트가 macOS에서 실제로 관찰한 것처럼, 좀비만 남은
/// 프로세스 그룹에 SIGKILL을 보내면 `EPERM`까지 날 수 있다 — 시그널을 보낼 대상이
/// 실질적으로 없기 때문으로 보인다) 문제를 알아챈다. 이 함수는 그 오판을 없앤다.
///
/// ## 왜 `waitid(WNOWAIT)`인가 — 그리고 왜 회수하지 않는가
/// `waitid(P_PID, pid, &info, WEXITED | WNOHANG | WNOWAIT)`는 POSIX가 명시적으로 제공하는
/// "엿보기" 관용구다(`waitid(2)` 매뉴얼이 권장하는 패턴: `siginfo_t`를 0으로 채운 뒤 호출
/// 후 `si_pid`가 여전히 0이면 "아직 종료 안 함", 그 pid로 채워졌으면 "종료함"). `WNOWAIT`
/// 없이 `waitpid`/`try_wait`를 쓰면 성공하는 순간 좀비를 **회수**해버리는데, 그 회수는
/// 다른 태스크(t11의 스트리밍 종료 감지, 또는 재부착된 잡이면 [`wait_for_orphan_exit`]을
/// 부른 쪽)의 몫이다([`super::runner`] 헤더: "잡 하나마다 반드시 누군가 `wait()`를
/// 호출해야 한다"). 이 함수가 먼저 회수해버리면 그 태스크가 나중에 같은 pid를
/// `wait()`했을 때 "그런 자식 없음"(`ECHILD`)을 받아 종료를 영영 감지하지 못한다 — 그래서
/// 반드시 "회수하지 않는" API를 쓴다.
///
/// ## 우리 자식이 아니면(`ECHILD`) `None`
/// `waitid`는 **직계 자식에만** 동작한다. 재부착된 잡은 우리 자식이 아니므로(재시작 전
/// 서버 프로세스는 이미 사라졌다) `ECHILD`가 난다 — 호출부는 [`pid_alive`]로 폴백해야
/// 한다([`pid_finished`] 참조). 진짜 고아의 좀비는 새 부모(보통 init/launchd, pid 1)가
/// 신속히 회수하므로(그게 init의 일이다) `kill(pid, 0)` 기반 판정이 좀비 상태로 오래
/// 머무를 위험은 낮다.
fn waitid_peek(pid: u32) -> Option<bool> {
    // SAFETY: info는 스택에 있고, waitid가 성공 시에만 채운다. 실패 시(예: ECHILD)
    // 내용을 읽지 않는다(0으로 미리 채워 뒀으므로 안전하긴 하지만, 애초에 읽지 않는다).
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let ret = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if ret != 0 {
        return None;
    }
    // WNOHANG인데 아직 상태 변화가 없으면 si_pid가 0으로 남는다(POSIX 관용구 — 위 doc).
    Some(siginfo_pid(&info) == pid as libc::pid_t)
}

/// pid가 "끝났다"고 볼 수 있는지 — [`cancel`]의 grace 폴링과 [`wait_for_orphan_exit`]이
/// 공유하는 판정. 우리 직계 자식이면 [`waitid_peek`](좀비를 정확히 감지), 아니면
/// [`pid_alive`](진짜 존재 확인)로 폴백한다. 자세한 근거는 [`waitid_peek`] doc 참조.
fn pid_finished(pid: u32) -> bool {
    match waitid_peek(pid) {
        Some(finished) => finished,
        None => !pid_alive(pid),
    }
}

/// [`send_group_signal`]의 결과 — "보냈다"와 "보내지 못했고 대상은 아직 살아 있다"를 가른다.
enum SignalOutcome {
    /// 시그널이 전달됐거나, 전달할 대상이 이미 사라졌다(둘 다 목표 달성).
    Sent,
    /// 커널이 전송을 거부했고(`EPERM`) **대상이 아직 살아 있다.** 사람이 읽을 사유를 담는다 —
    /// 호출부는 이것을 성공으로 접어서는 안 된다.
    RefusedAlive(String),
}

/// `EPERM`을 무엇으로 접을지 정한다 — **[`send_group_signal`] doc의 실측 근거가 "좀비만 남은
/// 그룹"에 한정된다는 사실을 코드로 표현하는 함수.**
///
/// 예전에는 `EPERM`을 무조건 `Ok(())`로 접었다. 실측 근거는 좀비 경로에 한정된 XNU 특이
/// 동작인데 코드는 그 조건을 전혀 확인하지 않았고, 그래서 **대상이 살아 있는데도 취소가
/// 성공으로 보고되는** 경로가 있었다: SIGTERM·SIGKILL 둘 다 `EPERM`인데 프로세스가 계속 돌면
/// [`cancel`]이 [`CancelOutcome::KilledAfterGrace`](= 취소 완료)를 반환하고 화면은 취소됐다고
/// 표시한다. 운영자는 잡이 멈춘 줄 알고 다음 행동으로 넘어가는데 백업은 계속 돌고 락도 그대로다.
///
/// 그래서 조건을 실제로 확인한다: [`pid_finished`]가 참이면 그것이 doc의 그 좀비 상황이므로
/// 관용을 적용하고, 거짓이면(살아 있으면) 거부로 접는다. `finished`를 인자로 받는 이유는
/// **테스트가 두 갈래를 모두 지나갈 수 있게** 하기 위해서다 — 실제 `EPERM`은 남의 사용자
/// 프로세스가 필요해 단위 테스트에서 만들 수 없다.
fn interpret_eperm(pid: u32, signal: libc::c_int, finished: bool) -> SignalOutcome {
    if finished {
        // doc의 실측 그대로 — 좀비만 남은 그룹에 대한 XNU의 권한 검사 특이 동작이다.
        return SignalOutcome::Sent;
    }
    SignalOutcome::RefusedAlive(format!(
        "pid {pid}(프로세스 그룹)에 시그널 {signal}을 보낼 권한이 없습니다(EPERM)이고 대상이 \
         아직 살아 있습니다 — 취소가 실제로 이뤄지지 않았습니다. 이 잡을 다른 사용자가 띄웠거나 \
         (예: cron이 root로 실행) 샌드박스가 시그널을 막고 있을 수 있습니다"
    ))
}

/// 프로세스 그룹(`-pid`)에 시그널을 보낸다.
///
/// [`super::runner`]가 자식을 `process_group(0)`으로 띄워 자기 그룹의 리더로 만들어
/// 두므로(그 파일 헤더), 음수 pid로 보내면 `mongodump`/`pg_dump` 같은 손자까지 함께
/// 정리된다. 대상이 이미 사라졌으면(`ESRCH`) 에러로 취급하지 않는다 — 이 함수의 목표는
/// "죽어 있게 만들기"이고, 이미 죽어 있으면 목표는 이미 달성된 것이다.
///
/// **`EPERM`은 [`interpret_eperm`]이 대상의 생존을 다시 확인해 가른다** — 아래 실측 근거는
/// "좀비만 남은 그룹"에만 적용되고, 대상이 살아 있는 `EPERM`은 취소 실패다. macOS에서
/// 자식이 이미 죽어 좀비만 남은 **프로세스 그룹**에 시그널을 보내면 `ESRCH`가 아니라
/// `EPERM`이 난다. 언뜻 "그 그룹번호가 남의 프로세스로 재사용됐다"는 신호처럼 보일 수
/// 있어 다음 순서로 직접 반증했다(같은 zombie 상태의 같은 pid에 대해):
///
/// 1. `kill(pid, 0)`(생존 확인) → 성공(좀비는 여전히 존재로 응답한다).
/// 2. `kill(pid, SIGTERM)`(**단일** pid, 그룹이 아님) → **성공.**
/// 3. `kill(pid, SIGKILL)`(역시 단일 pid) → **성공.**
/// 4. `kill(-pid, SIGKILL)`(**그룹**) → 여기서만 `EPERM`.
/// 5. 우리 `Child::try_wait()`로 실제 회수 → **우리 자식**의 종료 상태(`SIGTERM`로 종료)를
///    그대로 돌려준다 — 이 pid가 그 사이 다른 프로세스에 넘어간 적이 없다는 뜻이다(넘어갔다면
///    이 회수가 우리 자식의 진짜 종료 상태를 내놓을 수 없다 — `wait()`는 커널의 부모-자식
///    계보로만 동작하고 그 계보는 외부에서 가로챌 수 없다).
/// 6. 회수 **후** 같은 pid에 `kill(pid, 0)` → 그제서야 진짜 `ESRCH`.
///
/// **단일 pid 경로(2·3)가 정상 동작하는데 그룹 경로(4)만 `EPERM`이라는 것 자체가 "재사용된
/// 남의 프로세스"론을 반증한다** — 재사용이었다면 같은 번호를 가리키는 단일 pid 경로도
/// 이미 다른 프로세스를 건드리고 있었을 것이고, 애초에 좀비가 회수되기 전에는 커널이 그
/// pid/pgid를 다른 프로세스에 절대 재배정하지 않는다(POSIX 프로세스 테이블 불변식). 즉
/// 이건 **"좀비만 남은 그룹"에 대한 XNU의 그룹-시그널 권한 검사 특이 동작**이지 보안
/// 경계가 아니다. (샌드박스 특이사항이 아니라는 것도 같은 실측으로 확인했다 — 자식이
/// **살아있는 동안** 보낸 그룹 시그널(1단계에 해당하는 SIGTERM)은 아무 문제 없이
/// 성공했다. 샌드박스가 그룹 시그널 자체를 막는 정책이라면 살아있을 때도 막혔어야 한다.)
///
/// 다만 이 관용은 **실질적으로 거의 호출되지 않는 방어선**이다 — [`pid_finished`]가
/// [`wait_for_exit`]의 폴링에서 좀비를 이미 걸러내므로, 정상 경로에서는 대상이 좀비가 된
/// 시점에 이미 [`cancel`]이 `Terminated`로 반환하고 여기까지 오지 않는다. 이 분기는
/// 마지막 [`pid_finished`] 확인과 실제 `kill()` 호출 사이의 아주 좁은 경합(그 사이 자식이
/// 막 죽는 경우)을 흡수하는 안전망일 뿐이다. 그리고 이 관용을 타기 전에는 항상
/// [`verify_identity`]가 이미 OS의 독립적인 시작 시각으로 "이 pid가 정말 우리 잡"임을
/// 확인한 뒤다 — 두 겹의 근거(실측 + 사전 검증) 모두 이 `EPERM`이 재사용이 아님을
/// 가리킨다.
fn send_group_signal(pid: u32, signal: libc::c_int) -> Result<SignalOutcome> {
    // SAFETY: kill()은 시그널 전송만 하고 우리 메모리에 영향이 없다.
    let ret = unsafe { libc::kill(-(pid as libc::pid_t), signal) };
    if ret == 0 {
        return Ok(SignalOutcome::Sent);
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::EPERM) {
        // 좀비 경로에 한정된 관용인지 여기서 실제로 확인한다([`interpret_eperm`]).
        return Ok(interpret_eperm(pid, signal, pid_finished(pid)));
    }
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(SignalOutcome::Sent);
    }
    Err(XBackupError::Failure(format!(
        "pid {pid}(프로세스 그룹)에 시그널 {signal}을 보내지 못했습니다: {err}"
    )))
}

/// OS가 독립적으로 보고하는 프로세스의 실제 시작 시각(UTC) — 플랫폼별 한계는 모듈 헤더
/// 참조. 확인할 수 없으면 `None`(호출부는 이를 "불일치"와 동일하게 fail-closed 처리한다).
#[cfg(target_os = "linux")]
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    // /proc/<pid>/stat: `pid (comm) state ppid ...`. comm이 괄호로 감싸이고 그 안에
    // 공백·괄호가 있을 수 있어(프로세스 이름은 임의 바이트다) 마지막 ')' 뒤부터
    // 필드를 센다 — proc(5)가 권장하는 파싱 방법이다.
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = stat.rsplit_once(')')?.1;
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // comm 이후 0-based 인덱스: state(0) ppid(1) pgrp(2) session(3) tty_nr(4) tpgid(5)
    // flags(6) minflt(7) cminflt(8) majflt(9) cmajflt(10) utime(11) stime(12) cutime(13)
    // cstime(14) priority(15) nice(16) num_threads(17) itrealvalue(18) starttime(19).
    let starttime_ticks: i64 = fields.get(19)?.parse().ok()?;
    // SAFETY: sysconf는 순수 조회이고 인자·반환 모두 정수다.
    let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    if clk_tck <= 0 {
        return None;
    }
    let boot_time = linux_boot_time()?;
    let offset_ms = starttime_ticks.saturating_mul(1000) / clk_tck;
    Some(boot_time + chrono::Duration::milliseconds(offset_ms))
}

/// `/proc/stat`의 `btime`(부팅 시각, 유닉스 초) 줄을 읽는다.
#[cfg(target_os = "linux")]
fn linux_boot_time() -> Option<DateTime<Utc>> {
    let stat = std::fs::read_to_string("/proc/stat").ok()?;
    for line in stat.lines() {
        if let Some(rest) = line.strip_prefix("btime ") {
            let secs: i64 = rest.trim().parse().ok()?;
            return DateTime::<Utc>::from_timestamp(secs, 0);
        }
    }
    None
}

/// `/proc`가 없는 unix(주로 macOS) — `ps -o lstart=`를 셸 없이 서브프로세스로 띄워 파싱한다.
///
/// 한계는 모듈 헤더 "시작 시각을 어떻게 얻는가" 참조(초 단위 반올림, 로컬 시각→UTC 변환의
/// DST 경계 실패 가능성).
#[cfg(all(unix, not(target_os = "linux")))]
fn os_process_start_time(pid: u32) -> Option<DateTime<Utc>> {
    // `TimeZone`을 여기서 들여온다 — 이 trait은 아래 `Local.from_local_datetime` 한 곳만
    // 쓰고, 그 코드는 Linux에서 `cfg`로 빠진다. 파일 상단에 두면 **Linux 빌드에서만**
    // 미사용 import 경고가 뜨고, CI의 `clippy -D warnings`는 Linux에서 돌기 때문에 그
    // 경고가 곧 빌드 실패다(macOS에서 개발하는 동안에는 보이지 않는다).
    use chrono::TimeZone;

    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "lstart="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    // BSD/macOS ps의 lstart 형식(ctime 계열): "Sat Jul 25 13:52:53 2026". 로컬 시각이다.
    let naive = chrono::NaiveDateTime::parse_from_str(trimmed, "%a %b %e %H:%M:%S %Y").ok()?;
    // 모호하거나(가을 DST 전환) 존재하지 않는(봄 DST 전환) 로컬 시각이면 `single()`이
    // `None`을 준다 — 그 경우 확인 불가로 처리한다(호출부가 fail-closed로 거부한다).
    let local = chrono::Local.from_local_datetime(&naive).single()?;
    Some(local.with_timezone(&Utc))
}

/// pid의 명령행(argv)에 `needle`이 인자 하나로 그대로 있는지 — Linux는
/// `/proc/<pid>/cmdline`(NUL로 구분된 진짜 argv)을 읽는다.
///
/// 확인 자체가 실패하면(권한·부재) `None`, 확인은 됐지만 없으면 `Some(false)`.
#[cfg(target_os = "linux")]
fn os_process_cmdline_contains(pid: u32, needle: &str) -> Option<bool> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    if raw.is_empty() {
        // 커널이 이미 정리했거나(막 죽은 프로세스) 권한 문제 — 확인 불가로 취급한다.
        return None;
    }
    Some(raw.split(|&b| b == 0).any(|arg| arg == needle.as_bytes()))
}

/// `/proc`가 없는 unix — `ps -o args=`(폭 무제한 `-ww`)로 명령행 문자열을 얻어 토큰
/// 단위로 비교한다. argv 경계가 완벽히 보존되지는 않지만(공백으로만 나뉜 텍스트),
/// 프로파일명은 `-`로 시작하지 않고 공백을 포함할 수 없으므로([`super::args::ProfileName`]
/// 규칙) 토큰 비교로 충분하다.
#[cfg(all(unix, not(target_os = "linux")))]
fn os_process_cmdline_contains(pid: u32, needle: &str) -> Option<bool> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-ww", "-o", "args="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    if text.trim().is_empty() {
        return None;
    }
    Some(text.split_whitespace().any(|tok| tok == needle))
}

// ---------------------------------------------------------------------------
// 신원 재확인 — cancel과 classify_for_reattach가 공유하는 핵심 판정
// ---------------------------------------------------------------------------

/// [`verify_identity`]의 판정 결과.
enum Verified {
    /// 살아 있고 시작 시각이 일치한다 — 우리가 기록한 그 프로세스로 본다.
    Matches,
    /// 이미 죽어 있다(pid 부재) — 시그널을 보낼 대상이 없다.
    AlreadyExited,
    /// 살아는 있지만 시작 시각이 우리 기록과 어긋난다(또는 확인 자체가 불가능하다) —
    /// pid 재사용으로 의심해 거부한다. 사람이 읽을 사유를 담는다.
    Refused(String),
}

/// pid·시작 시각·명령행의 **3축 검증** — [`cancel`]과 [`classify_for_reattach`]가 공유하는
/// 유일한 판정 함수.
///
/// 두 호출부가 같은 함수를 쓰는 것이 요점이다. 둘 다 **디스크에 남은 기록만 보고** 판단하므로
/// (모듈 헤더 "취소도 3축을 요구한다") 적용할 기준이 같아야 하고, 함수가 하나면 한쪽만
/// 느슨해지는 일이 구조적으로 불가능하다.
///
/// 생존 확인에 [`pid_alive`]가 아니라 [`pid_finished`]를 쓴다 — 대상이 우리 직계 자식일
/// 수 있고, 그러면 스스로 끝났지만 아직 회수되지 않은 좀비를 "살아 있다"로 오판하지
/// 않아야 한다([`pid_finished`] doc의 좀비 문제 참조).
///
/// 축을 이 순서로 보는 이유: 1축(생존)은 시스템 콜 한 번으로 사실상 공짜이고, 죽어 있으면
/// 나머지를 볼 필요가 없다. 2·3축은 각각 파일 읽기 또는 `ps` 서브프로세스를 요구하므로
/// 앞 축이 걸러낸 뒤에만 낸다.
fn verify_identity(pid: u32, expected_started_at: DateTime<Utc>, profile: &str) -> Verified {
    if pid_finished(pid) {
        return Verified::AlreadyExited;
    }
    match os_process_start_time(pid) {
        Some(actual) => {
            let diff_ms = (actual - expected_started_at).num_milliseconds().abs();
            if diff_ms > START_TIME_TOLERANCE_MS {
                return Verified::Refused(format!(
                    "pid {pid}의 실제 시작 시각({actual})이 기록된 시작 시각\
                     ({expected_started_at})과 {diff_ms}ms 차이가 납니다 — pid 재사용으로 \
                     판단해 신호를 보내지 않습니다"
                ));
            }
        }
        None => {
            return Verified::Refused(format!(
                "pid {pid}의 OS 시작 시각을 확인할 수 없습니다(플랫폼 미지원이거나 조회 \
                 실패) — 안전을 위해 신호를 보내지 않습니다"
            ))
        }
    }
    match os_process_cmdline_contains(pid, profile) {
        Some(true) => Verified::Matches,
        Some(false) => Verified::Refused(format!(
            "pid {pid}는 살아 있고 시작 시각도 일치하지만, 명령행에 프로파일 '{profile}'이 \
             없습니다 — 우연히 같은 pid·시각을 가진 다른 프로세스로 판단해 신호를 보내지 \
             않습니다"
        )),
        None => Verified::Refused(format!(
            "pid {pid}의 명령행을 확인할 수 없어 프로파일 '{profile}' 일치를 검증하지 \
             못했습니다 — 안전을 위해 신호를 보내지 않습니다"
        )),
    }
}

// ---------------------------------------------------------------------------
// 취소 — SIGTERM → grace → SIGKILL
// ---------------------------------------------------------------------------

/// [`cancel`]의 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelOutcome {
    /// 시그널을 보내기 전에 이미 죽어 있었다 — 아무 것도 하지 않았다(멱등의 근거).
    AlreadyExited,
    /// SIGTERM만으로 grace 안에 종료됐다.
    Terminated,
    /// grace 타임아웃까지도 살아 있어 SIGKILL로 승격했다.
    KilledAfterGrace,
    /// 신원 확인에 실패해(죽음이 아니라 **불일치**) 어떤 시그널도 보내지 않았다. 사람이
    /// 읽을 사유를 담는다 — pid 재사용 의심(시작 시각·명령행 불일치)이 가장 흔한 원인이다.
    Refused(String),
}

impl CancelOutcome {
    /// 이 결과에 이르기까지 우리가 **실제로 보낸** 종료 시그널 번호(보냈다면).
    ///
    /// 호출부가 잡 이력에 종료 결과를 남길 때 쓴다
    /// ([`crate::web::routes::backup::cancel`]) — 취소로 죽은 잡의 진짜 결과는
    /// [`crate::web::job::JobOutcome::Signaled`]이고, 그 payload가 SIGTERM인지 SIGKILL인지는
    /// **이 파일만 안다**(호출부가 `libc` 상수를 다시 들여다볼 이유를 만들지 않는다).
    ///
    /// [`CancelOutcome::AlreadyExited`]는 `None`이다 — 우리가 아무 신호도 보내지 않았고,
    /// 그 잡이 어떻게 끝났는지는 이 함수가 알 수 있는 정보가 아니다.
    /// [`CancelOutcome::Refused`]도 `None`이다 — 대상은 **여전히 살아 있을 수 있다.**
    pub fn terminating_signal(&self) -> Option<i32> {
        match self {
            Self::Terminated => Some(libc::SIGTERM),
            Self::KilledAfterGrace => Some(libc::SIGKILL),
            Self::AlreadyExited | Self::Refused(_) => None,
        }
    }
}

/// 잡을 취소한다 — SIGTERM을 보내고, `grace` 동안 종료를 기다리고, 그래도 살아 있으면
/// SIGKILL로 승격한다.
///
/// 시그널을 보내기 **전에** [`verify_identity`]로 pid·시작 시각·명령행 3축을 재확인한다
/// (모듈 헤더 "최악의 실패는..."과 "취소도 3축을 요구한다" 참조) — 확인에 실패하면
/// [`CancelOutcome::Refused`]를 반환할 뿐 어떤 프로세스에도 손대지 않는다.
///
/// **멱등이다.** 이미 죽은 pid에 다시 부르면 [`CancelOutcome::AlreadyExited`]를 반환하고
/// 아무 신호도 보내지 않는다 — 우리 자신의 자식이 아직
/// [`super::runner::RunningJob::wait`]로 수거되지 않은 좀비 상태여도 마찬가지다
/// ([`verify_identity`]가 [`pid_finished`]로 좀비를 정확히 "끝남"으로 판정한다 — 단순히
/// `kill(pid, 0)`만 썼다면 좀비도 존재로 보여 오판했을 것이다, [`pid_finished`] doc 참조).
///
/// 이 함수는 async지만 SIGTERM/SIGKILL 전송 자체는 즉시 반환하는 시스템 콜이다 — 시간이
/// 걸리는 부분은 `grace` 동안의 폴링([`tokio::time::sleep`])이다.
pub async fn cancel(identity: &impl JobIdentity, grace: Duration) -> Result<CancelOutcome> {
    let pid = identity.pid();
    let expected_started_at = identity.started_at();

    match verify_identity(pid, expected_started_at, identity.profile()) {
        Verified::AlreadyExited => return Ok(CancelOutcome::AlreadyExited),
        Verified::Refused(reason) => return Ok(CancelOutcome::Refused(reason)),
        Verified::Matches => {}
    }

    // 전송이 거부되고 대상이 살아 있으면 취소는 이뤄지지 않았다 — 성공으로 접지 않는다(M4).
    if let SignalOutcome::RefusedAlive(reason) = send_group_signal(pid, libc::SIGTERM)? {
        return Ok(CancelOutcome::Refused(reason));
    }

    if wait_for_exit(pid, grace).await {
        return Ok(CancelOutcome::Terminated);
    }

    // grace 내내 매 폴마다 생존을 관찰했으므로(끊긴 적이 없다) 이 시점의 pid가 재사용된
    // 것일 수 없다 — 재검증 없이 승격한다(모듈 헤더 "grace 타임아웃과 SIGKILL 승격").
    if let SignalOutcome::RefusedAlive(reason) = send_group_signal(pid, libc::SIGKILL)? {
        // SIGTERM은 전달됐지만 SIGKILL이 거부되고 대상이 아직 살아 있다. 여기서
        // `KilledAfterGrace`를 반환하면 **취소 실패를 성공으로 보고하는 것**이다 — 운영자는
        // 잡이 멈춘 줄 알고 다음 행동으로 넘어가는데 백업은 계속 돌고 락도 그대로다.
        return Ok(CancelOutcome::Refused(format!(
            "SIGTERM은 보냈지만 grace({}s) 안에 끝나지 않았고 SIGKILL 승격이 거부됐습니다 — \
             {reason}",
            grace.as_secs()
        )));
    }
    // 결과와 무관하게 KilledAfterGrace를 보고한다 — 여기서 우리가 할 수 있는 조치는
    // 이미 다 했다(상수 KILL_CONFIRM_TIMEOUT doc 참조).
    let _ = wait_for_exit(pid, KILL_CONFIRM_TIMEOUT).await;
    Ok(CancelOutcome::KilledAfterGrace)
}

/// `timeout` 안에 pid가 끝나면 `true`, 타임아웃까지 살아 있으면 `false`. 좀비를 정확히
/// 감지하기 위해 [`pid_finished`]를 쓴다(doc 참조) — 그래야 SIGTERM으로 이미 죽은 자식을
/// grace 내내 "살아 있다"로 오판해 불필요하게 SIGKILL까지 승격하는 일이 없다.
async fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if pid_finished(pid) {
            return true;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return false;
        }
        tokio::time::sleep(CANCEL_POLL_INTERVAL.min(remaining)).await;
    }
}

/// **방금 spawn한 자식을 신원 재확인 없이** 정리한다 — 시그널을 보내고 **회수까지** 한다.
///
/// ## 왜 이 함수가 필요한가 — 배선 도중 실패하면 자식이 고아로 남는다
/// [`crate::web::routes::backup::spawn_tracked_job`]은 자식을 먼저 띄우고 그 다음에 잡 이력을
/// 기록한다. 그 사이에 기록이 실패하면(디스크 풀·권한) `RunningJob`이 `wait()` 없이 drop되고,
/// 그러면 [`super::runner`] 헤더가 명시한 계약("잡 하나마다 반드시 누군가 `wait()`를 호출해야
/// 한다")이 깨지는 **유일한 경로**가 열린다. 결과가 나쁘다: `kill_on_drop`이 의도적으로 꺼져
/// 있으므로 자식은 **락을 든 채 계속 돌고**(다음 백업이 exit 5로 막힌다), 파이프가 닫혀 다음
/// stdout 쓰기에서 죽은 뒤에는 서버 종료까지 **좀비로** 남는다. 이력에는 항목조차 없어서
/// 재부착 스캔([`crate::web::reattach`])도 그 잡을 찾을 수 없다 — 어떤 회복 경로도 닿지 않는다.
///
/// ## 왜 신원을 재확인하지 않는가 — 여기서는 3축이 오히려 약한 증거다
/// [`cancel`]이 3축을 요구하는 이유는 **디스크에 남은 기록만 보고** 판단하기 때문이다(모듈
/// 헤더). 이 함수는 정반대 상황이다: 호출부가 **살아 있는 [`super::runner::RunningJob`]을 값으로
/// 넘긴다.** 그 안의 `Child`를 우리가 쥐고 있는 동안 커널은 그 pid를 절대 다른 프로세스에
/// 재배정하지 않는다(회수되지 않은 자식의 pid는 재사용될 수 없다 — POSIX 프로세스 테이블
/// 불변식). 즉 pid 재사용이라는 위험 자체가 **타입 수준에서 존재하지 않는다.** OS에 시작
/// 시각을 다시 물어보는 것은 그보다 약한 증거를 덧붙이는 것이고, 실패할 수도 있어(플랫폼 한계)
/// 정리해야 할 자식을 정리하지 못하게 만들 뿐이다.
///
/// ## 파이프를 먼저 닫는다
/// 시그널을 보내기 전에 stdout/stderr를 꺼내 drop한다. 두 가지를 얻는다: (1) 자식이 파이프
/// 버퍼가 차서 write에 블록해 있어도 `EPIPE`/`SIGPIPE`로 즉시 풀린다 — [`super::runner`] 헤더의
/// "파이프를 비우지 않으면 자식이 멈춘다"가 이 경로에서 `wait()` 교착으로 나타나는 것을 막는다.
/// (2) 이 자식의 출력은 어디에도 중계되지 않으므로(이력 항목이 없다) 읽어 둘 이유가 없다.
///
/// 반환은 [`CancelOutcome`]이지만 `Refused`는 나올 수 없다 — 신원 확인을 하지 않으므로.
/// 회수(`wait()`)에 실패하면 그것만 별도로 보고한다.
pub async fn terminate_just_spawned(
    mut running: super::runner::RunningJob,
    grace: Duration,
) -> Result<CancelOutcome> {
    // (1) 파이프를 닫는다 — 위 doc "파이프를 먼저 닫는다".
    drop(running.take_stdout());
    drop(running.take_stderr());

    let Some(pid) = running.handle().pid else {
        // 이미 회수됐다 = 우리가 할 일이 없다(정상 경로에서는 도달하지 않는다).
        return Ok(CancelOutcome::AlreadyExited);
    };

    let outcome = if pid_finished(pid) {
        CancelOutcome::AlreadyExited
    } else {
        match send_group_signal(pid, libc::SIGTERM)? {
            SignalOutcome::Sent => {}
            // 우리 자식인데도 전송이 거부되고 살아 있다 — SIGKILL을 시도할 가치가 있다.
            SignalOutcome::RefusedAlive(reason) => {
                tracing::warn!(pid, reason = %reason, "방금 띄운 자식에 SIGTERM을 보내지 못했습니다 — SIGKILL을 시도합니다");
            }
        }
        if wait_for_exit(pid, grace).await {
            CancelOutcome::Terminated
        } else {
            if let SignalOutcome::RefusedAlive(reason) = send_group_signal(pid, libc::SIGKILL)? {
                tracing::warn!(pid, reason = %reason, "방금 띄운 자식에 SIGKILL을 보내지 못했습니다");
            }
            let _ = wait_for_exit(pid, KILL_CONFIRM_TIMEOUT).await;
            CancelOutcome::KilledAfterGrace
        }
    };

    // (2) 반드시 회수한다 — 이 함수의 존재 이유의 절반이다. 좀비를 남기면 애초에 고치려던
    // 문제가 그대로 남는다.
    running.wait().await?;
    Ok(outcome)
}

// ---------------------------------------------------------------------------
// 재부착 — pid + 시작 시각 + 프로파일 3중 일치
// ---------------------------------------------------------------------------

/// [`classify_for_reattach`]의 결과.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReattachDecision {
    /// 3중 일치 — 재부착한다. 진행률은 [`REATTACHED_PROGRESS_LABEL`]로 "미상"으로
    /// 표시하고, 종료 감지는 호출부가 [`wait_for_orphan_exit`]을 부른다.
    Reattached,
    /// 셋 중 하나라도 어긋났다 — crashed로 처리한다. 사유를 담는다(진단·화면 표시용).
    Crashed(String),
}

/// 서버 기동 시 상태에 남은 잡 기록 하나를 재부착 가능한지 판정한다.
///
/// [`verify_identity`]의 3축(pid 생존 + 시작 시각 + 명령행의 프로파일)이 모두 맞아야
/// [`ReattachDecision::Reattached`]다. [`cancel`]과 **같은 판정 함수**를 쓴다 — 둘 다
/// 디스크에 남은 기록만 보고 판단하므로 기준이 같아야 한다(모듈 헤더 "취소도 3축을
/// 요구한다").
///
/// 동기 함수다 — Linux 경로는 파일 두 개를 읽는 정도라 빠르고, macOS 폴백은 `ps`
/// 서브프로세스를 띄우므로 짧게 블로킹한다. 서버 기동 시 잡 개수만큼만(보통 0~수 개) 한
/// 번씩 불리므로 비동기화의 이득이 작다고 판단했다 — 호출부가 async 문맥에서 다수의
/// 기록을 처리하며 블로킹을 피하고 싶다면 `tokio::task::spawn_blocking`으로 감싸는 것을
/// 권장한다.
pub fn classify_for_reattach(identity: &impl JobIdentity) -> ReattachDecision {
    let pid = identity.pid();
    let expected_started_at = identity.started_at();
    let profile = identity.profile();

    match verify_identity(pid, expected_started_at, profile) {
        Verified::Matches => ReattachDecision::Reattached,
        Verified::AlreadyExited => {
            ReattachDecision::Crashed(format!("pid {pid}가 더 이상 존재하지 않습니다"))
        }
        Verified::Refused(reason) => ReattachDecision::Crashed(reason),
    }
}

/// 재부착된 잡의 pid가 사라질 때까지 폴링한다.
///
/// 재부착된 잡은 stdout 파이프를 잃어(모듈 헤더 "재부착 — 진행률을 잃는다") 종료를
/// [`super::runner::RunningJob::wait`]처럼 `wait(2)`로 알 수 없다(우리 자식이 아니므로
/// `wait(2)`는 애초에 이 pid에 쓸 수 없다 — `wait(2)`는 **직계 자식에만** 동작한다).
/// [`pid_finished`]로 폴링한다 — 진짜 고아라면 내부적으로 [`pid_alive`] 폴백 경로를 타고
/// (재부모인 init/launchd가 좀비를 신속히 회수하므로 신뢰할 수 있다), 혹시라도 이 함수가
/// 아직 우리 자식인 pid에 불렸다면 좀비 오판 없이도 정확히 동작한다. 간격의 근거는
/// [`REATTACH_POLL_INTERVAL`] doc 참조.
///
/// 반환은 pid가 사라진 뒤(더 이상 살아있지 않을 때)다 — 종료 코드는 알 수 없다(재부착의
/// 근본적 한계, 모듈 헤더 참조). 호출부는 이 함수가 끝나면 화면 상태를 "재부착됨(진행률
/// 미상)"에서 "완료(결과 불명)"로 옮기면 된다.
pub async fn wait_for_orphan_exit(pid: u32, poll_interval: Duration) {
    loop {
        if pid_finished(pid) {
            return;
        }
        tokio::time::sleep(poll_interval).await;
    }
}

// ---------------------------------------------------------------------------
// 테스트
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use std::path::PathBuf;

    /// `/bin/sleep <N>` 자식을 쓰는 취소 테스트들이 "프로파일"로 쓰는 토큰.
    ///
    /// 3축 검증(모듈 헤더 "취소도 3축을 요구한다")은 명령행에 프로파일 이름이 인자로 있는지
    /// 본다. 프로덕션에서는 `--profile <name>`이 그 자리를 채우지만, 여기서는 `sleep`의
    /// 지속 시간 인자를 그 자리에 세운다 — 검증하려는 것은 "살아있는 실제 프로세스의 명령행을
    /// 읽어 토큰이 있는지 판정하는 메커니즘"이고, 그 성질은 토큰이 무엇이든 같다
    /// (`classify_for_reattach` 테스트들이 이미 쓰던 관용구다). 값이 초 단위 지속 시간으로도
    /// 유효해야 하므로 숫자다.
    const SLEEP_PROFILE_TOKEN: &str = "30";

    /// SIGTERM을 무시하는 self-exec 워커에게 argv로 심어 주는 프로파일 토큰
    /// ([`spawn_self_exec_worker`] doc 참조).
    const IGNORE_TERM_PROFILE: &str = "lifecycle-ignore-term";

    // -----------------------------------------------------------------
    // self-exec 워커 — 셸 없이 "SIGTERM을 실제로 처리하는 자식"·"SIGTERM을 무시하는
    // 자식"을 만들기 위한 장치.
    //
    // 표준 유닉스 도구만으로는 두 성질을 셸(trap) 없이 만들 수 없다(`/bin/sleep`은
    // SIGTERM에 기본 동작으로 즉시 죽어 "lock을 정리하며 종료"를 보여줄 수 없고,
    // "SIGTERM을 무시"하는 표준 바이너리도 없다). 그래서 **이 테스트 바이너리 자신을
    // 다시 실행**해 특정 #[test] 함수 하나만 골라 돌리는 관용구를 쓴다(자기 자신을
    // fork+exec하므로 `Command::spawn()`의 표준 안전성을 그대로 쓰고, 셸을 전혀
    // 거치지 않는다). 아래 두 "워커" 함수는 환경변수가 없으면 즉시 반환하므로 평범한
    // `cargo test --lib` 전체 실행에서는 무해한 통과 테스트로 남는다.
    // -----------------------------------------------------------------

    /// self-exec 워커: 진짜 [`crate::lock`]을 잡고, SIGTERM을 **정상 종료 경로**로 받아
    /// lock을 드롭한 뒤 반환한다. `sigterm_stops_a_lock_holder_and_removes_the_lock_file`이
    /// 서브프로세스로 재실행한다.
    #[test]
    fn lifecycle_worker_lock_and_wait_for_term() {
        let Ok(dir) = std::env::var("XB_LIFECYCLE_TEST_LOCK_DIR") else {
            return; // 워커 모드가 아니다 — 평범한 테스트 실행에서는 아무 것도 안 한다.
        };
        let profile = std::env::var("XB_LIFECYCLE_TEST_PROFILE")
            .unwrap_or_else(|_| "lifecycle-worker".to_string());

        // 락은 자식이 잡는다(runner.rs 헤더의 그 원칙) — 이 워커는 "잘 작동하는 자식"을
        // 흉내내는 테스트 전용 대역일 뿐, 프로덕션 취소 로직(cancel())은 락을 전혀
        // 건드리지 않는다.
        let guard = crate::lock::acquire_in(std::path::Path::new(&dir), &profile)
            .expect("워커가 lock을 잡지 못했습니다");

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("워커 런타임 생성 실패");
        rt.block_on(async {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("SIGTERM 리스너 설치 실패");
            term.recv().await;
        });

        drop(guard); // 정상 반환 경로 — Drop이 lock 파일을 지운다.
    }

    /// self-exec 워커: SIGTERM을 명시적으로 무시하고 영원히 잔다. SIGKILL만이 이 프로세스를
    /// 끝낼 수 있다 — grace 타임아웃 후 SIGKILL 승격 경로를 실제 자식으로 검증하기 위한
    /// 장치다.
    #[test]
    fn lifecycle_worker_ignore_term_forever() {
        if std::env::var_os("XB_LIFECYCLE_TEST_IGNORE_TERM").is_none() {
            return;
        }
        // SAFETY: signal(2)은 이 프로세스의 시그널 처리(disposition)만 바꾼다.
        unsafe {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        loop {
            std::thread::sleep(Duration::from_secs(3600));
        }
    }

    /// 현재 테스트 바이너리를 재실행해 `test_name` 하나만 돌리는 자식을 띄운다(셸 없음).
    ///
    /// `argv_tokens`는 **3축 검증을 위해 argv에 심는 토큰**이다(보통 프로파일 이름 하나).
    /// [`cancel`]이 명령행에서 프로파일을 찾으므로(모듈 헤더 "취소도 3축을 요구한다") 워커
    /// 자식도 프로덕션 자식처럼 프로파일을 argv에 들고 있어야 한다. 환경변수로 넘기면
    /// `/proc/<pid>/cmdline`·`ps -o args=`에 나타나지 않아 검증을 통과하지 못한다.
    ///
    /// libtest에 여분의 위치 인자를 넘기는 것은 안전하다 — 위치 인자는 **필터 목록**이고
    /// (여러 개를 OR로 받는다), `--exact`가 붙어 있으므로 프로파일 이름과 정확히 같은 이름의
    /// 테스트가 없는 한 추가 테스트가 함께 돌지 않는다.
    fn spawn_self_exec_worker(
        test_name: &str,
        argv_tokens: &[&str],
        envs: &[(&str, &str)],
    ) -> tokio::process::Child {
        let exe = std::env::current_exe().expect("현재 테스트 바이너리 경로를 얻지 못했습니다");
        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg(test_name);
        cmd.args(argv_tokens);
        cmd.arg("--exact");
        for (k, v) in envs {
            cmd.env(k, v);
        }
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn().expect("워커 spawn 실패")
    }

    /// `predicate`가 참이 될 때까지 `timeout` 안에서 짧게 폴링한다(테스트 전용 대기 유틸).
    async fn wait_until(timeout: Duration, mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if predicate() {
                return true;
            }
            if tokio::time::Instant::now() >= deadline {
                return false;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// 프로파일 lock 파일 경로 — `crate::lock::file_lock`의 `<dir>/<profile>.lock` 규칙을
    /// 테스트에서만 재현한다(그 함수는 `pub`이 아니다).
    fn lock_file_path(dir: &std::path::Path, profile: &str) -> PathBuf {
        dir.join(format!("{profile}.lock"))
    }

    // -----------------------------------------------------------------
    // pid_alive
    // -----------------------------------------------------------------

    #[test]
    fn pid_alive_true_for_self() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    fn pid_alive_false_for_nonexistent_pid() {
        assert!(!pid_alive(u32::MAX - 1));
    }

    // -----------------------------------------------------------------
    // JobRecord::from_handle
    // -----------------------------------------------------------------

    #[test]
    fn from_handle_none_when_pid_already_reaped() {
        let handle = super::super::runner::JobHandle {
            pid: None,
            started_at: Utc::now(),
            command: "backup",
            profile: Some("p".to_string()),
        };
        assert!(JobRecord::from_handle(&handle).is_none());
    }

    #[test]
    fn from_handle_none_when_profile_missing() {
        let handle = super::super::runner::JobHandle {
            pid: Some(123),
            started_at: Utc::now(),
            command: "list",
            profile: None,
        };
        assert!(JobRecord::from_handle(&handle).is_none());
    }

    #[test]
    fn from_handle_some_when_pid_and_profile_present() {
        let started = Utc::now();
        let handle = super::super::runner::JobHandle {
            pid: Some(4242),
            started_at: started,
            command: "backup",
            profile: Some("prod".to_string()),
        };
        let record = JobRecord::from_handle(&handle).expect("변환 실패");
        assert_eq!(record.pid, 4242);
        assert_eq!(record.started_at, started);
        assert_eq!(record.profile, "prod");
    }

    // -----------------------------------------------------------------
    // cancel — 실제 자식 프로세스
    // -----------------------------------------------------------------

    /// 이미 죽은(수거된) pid에는 신호를 보내지 않는다 — 조용히 `AlreadyExited`.
    #[tokio::test]
    async fn cancel_already_exited_pid_is_a_no_op() {
        let mut child = tokio::process::Command::new("/bin/sh")
            .args(["-c", "true"]) // 존재하는지만 필요하므로 아무 셸이나 무방(취소 대상 자체가 아님).
            .spawn()
            .expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let status = child.wait().await.expect("wait 실패");
        assert!(status.success());
        // 수거 완료 — 이 pid는 이제 우리 프로세스 테이블에서 사라졌다(재사용 가능).
        assert!(!pid_alive(pid), "테스트 전제: 수거 후에는 죽어 있어야 함");

        let identity = JobRecord {
            pid,
            started_at: Utc::now(),
            profile: "irrelevant".to_string(),
        };
        let outcome = cancel(&identity, Duration::from_millis(500))
            .await
            .expect("cancel 자체가 에러를 반환하면 안 됨");
        assert_eq!(outcome, CancelOutcome::AlreadyExited);
    }

    /// SIGTERM만으로 grace 안에 종료되는 협조적 자식은 `Terminated`로 보고된다.
    ///
    /// `profile`을 `SLEEP_PROFILE_TOKEN`(= sleep에 넘긴 인자)으로 두는 이유는 그 상수 doc
    /// 참조 — 3축 검증이 명령행에서 프로파일 토큰을 찾기 때문이다.
    #[tokio::test]
    async fn cancel_terminates_a_cooperative_child_within_grace() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE_TOKEN);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let started_at = Utc::now();

        let identity = JobRecord {
            pid,
            started_at,
            profile: SLEEP_PROFILE_TOKEN.to_string(),
        };
        let outcome = cancel(&identity, Duration::from_secs(5))
            .await
            .expect("cancel 실패");
        assert_eq!(outcome, CancelOutcome::Terminated);

        let status = child.wait().await.expect("wait 실패");
        assert!(
            !status.success(),
            "SIGTERM으로 죽은 프로세스는 성공 종료가 아니다"
        );
    }

    /// **가장 중요한 테스트**: pid는 실제로 살아 있는 프로세스를 가리키지만, 기록된 시작
    /// 시각이 그 프로세스의 실제 시작 시각과 다르면(= pid 재사용 시나리오) 신호를 전혀
    /// 보내지 않는다 — 취소 요청 하나가 무관한 프로세스를 죽이지 않는다는 핵심 불변식.
    #[tokio::test]
    async fn cancel_refuses_when_started_at_does_not_match_a_real_alive_process() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE_TOKEN);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");

        // 실제 시작 시각과 명백히 다른(1시간 전) 값을 "기록된 시작 시각"으로 준다 —
        // "pid는 같지만 다른 프로세스"를 흉내낸다. 3축 중 시작 시각 축만 어긋나게 두어
        // (프로파일 토큰은 맞다) 그 축 하나만으로도 거부된다는 것을 보인다.
        let wrong_started_at = Utc::now() - chrono::Duration::hours(1);
        let identity = JobRecord {
            pid,
            started_at: wrong_started_at,
            profile: SLEEP_PROFILE_TOKEN.to_string(),
        };

        let outcome = cancel(&identity, Duration::from_secs(2))
            .await
            .expect("cancel 자체가 에러를 반환하면 안 됨");
        match outcome {
            CancelOutcome::Refused(reason) => {
                assert!(
                    reason.contains("pid"),
                    "사유에 pid 언급이 있어야 함: {reason}"
                );
            }
            other => panic!("불일치 시 Refused여야 하는데 {other:?}가 나왔습니다"),
        }

        // 진짜 확인: 프로세스가 여전히 살아 있다(우리가 건드리지 않았다).
        assert_eq!(
            child.try_wait().expect("try_wait 실패"),
            None,
            "불일치 판정에도 불구하고 프로세스가 종료됐다 — 잘못 시그널을 보낸 것이다"
        );

        // 정리: 이번엔 올바른 시작 시각으로 실제 취소한다.
        let real_identity = JobRecord {
            pid,
            started_at: child_started_at_hint(),
            profile: SLEEP_PROFILE_TOKEN.to_string(),
        };
        let _ = cancel(&real_identity, Duration::from_secs(2)).await;
        let _ = child.kill().await; // 만에 하나 남아 있으면 정리(테스트 리소스 누수 방지).
        let _ = child.wait().await;
    }

    /// **H2 회귀 방어**: pid도 살아 있고 시작 시각도 맞지만 **명령행에 프로파일이 없으면**
    /// 취소는 아무 시그널도 보내지 않는다.
    ///
    /// 이게 왜 중요한가: 예전 [`cancel`]은 2축(pid + 시작 시각)만 봤다. 그 두 축은 pid가
    /// 회수·재사용되고 새 프로세스가 우연히 기록값 ±[`START_TIME_TOLERANCE_MS`] 안에 뜨면
    /// 통과한다 — 그 순간 `kill(-pid, SIGTERM)`이 **남의 프로세스 그룹 전체**로 간다. 이
    /// 테스트가 재현하는 것이 정확히 그 상황이다: 살아있는 실제 프로세스에, 실제 시작 시각을
    /// 그대로 주고, 프로파일만 그 프로세스의 argv에 없는 값으로 준다. 3축이 아니면 이 호출이
    /// 프로세스를 죽인다.
    #[tokio::test]
    async fn cancel_refuses_when_cmdline_does_not_carry_the_profile() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE_TOKEN);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        // 1·2축은 진짜다 — 방금 우리가 띄운 프로세스의 pid와 시작 시각이다.
        let identity = JobRecord {
            pid,
            started_at: Utc::now(),
            profile: "definitely-not-in-argv".to_string(),
        };

        let outcome = cancel(&identity, Duration::from_secs(2))
            .await
            .expect("cancel 자체가 에러를 반환하면 안 됨");
        match outcome {
            CancelOutcome::Refused(reason) => {
                assert!(
                    reason.contains("definitely-not-in-argv"),
                    "사유에 프로파일 불일치가 드러나야 함: {reason}"
                );
            }
            other => panic!("프로파일 불일치인데 {other:?}가 나왔습니다 — 남의 프로세스에 신호를 보낼 수 있는 상태다"),
        }

        // 진짜 확인: 프로세스가 여전히 살아 있다(우리가 건드리지 않았다).
        assert_eq!(
            child.try_wait().expect("try_wait 실패"),
            None,
            "3축 불일치인데 프로세스가 종료됐다 — 잘못 시그널을 보낸 것이다"
        );

        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// **M4 회귀 방어**: `EPERM`을 성공으로 접는 관용은 **좀비 경로에만** 적용된다.
    ///
    /// doc의 실측 근거는 "좀비만 남은 그룹"에 대한 XNU 특이 동작이지만, 예전 코드는 조건을
    /// 확인하지 않고 `EPERM`을 전부 `Ok`로 접었다. 그러면 대상이 **살아 있는데도** SIGTERM·
    /// SIGKILL 둘 다 거부된 경우에 [`cancel`]이 `KilledAfterGrace`(= 취소 완료)를 반환한다 —
    /// 화면은 취소됐다고 표시하고 운영자는 다음 행동으로 넘어가는데 백업은 계속 돌고 락도
    /// 그대로다. 이 테스트는 그 판정([`interpret_eperm`])의 두 갈래를 모두 지나간다.
    ///
    /// 실제 `EPERM`은 남의 사용자 프로세스가 필요해 단위 테스트로 만들 수 없다 — 그래서
    /// `finished`를 인자로 받는 순수 함수로 분리해 두었고, 이 테스트가 그 seam을 쓴다.
    #[test]
    fn eperm_is_only_forgiven_when_the_target_is_actually_gone() {
        // 끝났다 → doc의 그 좀비 상황. 관용을 적용한다.
        assert!(matches!(
            interpret_eperm(4242, libc::SIGKILL, true),
            SignalOutcome::Sent
        ));
        // 살아 있다 → 취소가 이뤄지지 않았다. 성공으로 접으면 안 된다.
        match interpret_eperm(4242, libc::SIGKILL, false) {
            SignalOutcome::RefusedAlive(reason) => {
                assert!(reason.contains("4242"), "사유에 pid가 없다: {reason}");
                assert!(
                    reason.contains("EPERM"),
                    "사유에 원인이 드러나야 한다: {reason}"
                );
            }
            SignalOutcome::Sent => {
                panic!("대상이 살아 있는 EPERM을 성공으로 접었다 — 취소 실패를 성공으로 보고한다")
            }
        }
    }

    /// 정상 경로의 [`send_group_signal`]은 `Sent`를 돌려준다 — 살아 있는 자식(전달됨)과
    /// 이미 사라진 pid(`ESRCH`, 목표 달성) 양쪽.
    #[tokio::test]
    async fn send_group_signal_reports_sent_on_the_normal_paths() {
        // 이미 사라진 pid — ESRCH.
        assert!(matches!(
            send_group_signal(u32::MAX - 1, libc::SIGTERM).expect("에러가 아니어야 함"),
            SignalOutcome::Sent
        ));

        // 살아 있는 우리 자식 — 실제 전달.
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE_TOKEN);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        assert!(matches!(
            send_group_signal(pid, libc::SIGTERM).expect("에러가 아니어야 함"),
            SignalOutcome::Sent
        ));
        let _ = child.wait().await;
    }

    /// 취소 결과가 "우리가 실제로 보낸 시그널"을 정확히 돌려준다 — 호출부가 잡 이력에
    /// [`crate::web::job::JobOutcome::Signaled`]로 남길 때 쓰는 값이다.
    ///
    /// `AlreadyExited`·`Refused`가 `None`인 것이 핵심이다: 전자는 우리가 아무것도 보내지
    /// 않았고, 후자는 **대상이 여전히 살아 있을 수 있다** — 둘 중 어느 경우에도 "시그널로
    /// 죽었다"는 이력을 남기면 거짓말이 된다.
    #[test]
    fn terminating_signal_reports_only_what_we_actually_sent() {
        assert_eq!(
            CancelOutcome::Terminated.terminating_signal(),
            Some(libc::SIGTERM)
        );
        assert_eq!(
            CancelOutcome::KilledAfterGrace.terminating_signal(),
            Some(libc::SIGKILL)
        );
        assert_eq!(CancelOutcome::AlreadyExited.terminating_signal(), None);
        assert_eq!(
            CancelOutcome::Refused("어떤 사유".to_string()).terminating_signal(),
            None
        );
    }

    /// 위 테스트의 정리 단계에서 쓰는, "지금 막 spawn했다"는 근사 시각. 이 테스트 파일
    /// 안에서 spawn 직후 캡처한 시각을 그대로 쓰는 다른 테스트들과 달리, 이미 spawn된
    /// 프로세스를 정리만 하는 용도이므로 관용적인 지금 시각으로 재확인을 통과시킨다.
    fn child_started_at_hint() -> DateTime<Utc> {
        Utc::now()
    }

    /// 취소를 두 번 요청해도 안전하다(멱등) — 첫 취소로 프로세스가 죽고 수거된 뒤, 같은
    /// 신원으로 다시 불러도 에러 없이 `AlreadyExited`를 반환한다.
    #[tokio::test]
    async fn cancel_twice_is_idempotent() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE_TOKEN);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let started_at = Utc::now();
        let identity = JobRecord {
            pid,
            started_at,
            profile: SLEEP_PROFILE_TOKEN.to_string(),
        };

        let first = cancel(&identity, Duration::from_secs(5))
            .await
            .expect("첫 cancel 실패");
        assert_eq!(first, CancelOutcome::Terminated);

        // 실제 시스템에서는 스트리밍 태스크(t11)나 재부착 폴링(wait_for_orphan_exit)이
        // 종료를 수거한다 — 여기서는 그 역할을 테스트가 직접 흉내낸다(zombie 회수).
        let _ = child.wait().await;

        let second = cancel(&identity, Duration::from_secs(5))
            .await
            .expect("두 번째 cancel 실패");
        assert_eq!(
            second,
            CancelOutcome::AlreadyExited,
            "재호출은 이미 죽은 pid에 신호를 보내지 않아야 한다"
        );
    }

    /// grace 타임아웃까지 SIGTERM을 무시하는 자식은 SIGKILL로 승격된다.
    #[tokio::test]
    async fn cancel_escalates_to_sigkill_after_grace_timeout() {
        let mut child = spawn_self_exec_worker(
            "web::job::lifecycle::tests::lifecycle_worker_ignore_term_forever",
            &[IGNORE_TERM_PROFILE],
            &[("XB_LIFECYCLE_TEST_IGNORE_TERM", "1")],
        );
        let pid = child.id().expect("pid 없음");
        // 워커가 signal(SIG_IGN)을 설치할 시간을 아주 짧게 준다(수 ms면 충분하지만,
        // 느린 CI를 감안해 넉넉히 잡는다).
        tokio::time::sleep(Duration::from_millis(200)).await;
        let started_at = Utc::now();

        let identity = JobRecord {
            pid,
            started_at,
            profile: IGNORE_TERM_PROFILE.to_string(),
        };
        // grace를 짧게 잡아 테스트 시간을 줄인다 — 이 값 자체가 프로덕션 기본값
        // (CANCEL_GRACE_DEFAULT)일 필요는 없다([`cancel`] doc 참조).
        let outcome = cancel(&identity, Duration::from_millis(300))
            .await
            .expect("cancel 실패");
        assert_eq!(outcome, CancelOutcome::KilledAfterGrace);

        let exited = wait_until(Duration::from_secs(2), || {
            matches!(child.try_wait(), Ok(Some(_)))
        })
        .await;
        assert!(exited, "SIGKILL 후에도 프로세스가 살아 있다");
        let _ = child.wait().await;
    }

    /// SIGTERM을 실제로 처리하는(=lock을 정상적으로 정리하고 종료하는) 자식을 대상으로,
    /// 취소가 lock 파일을 사라지게 만드는지 확인한다. 팀 요구사항 문구 그대로의 시나리오다
    /// — 다만 대상은 `x-backup backup`이 아니라 이 파일 자신의 워커다(모듈 헤더의 정직한
    /// 고지 참조: 현재 `backup`/`restore`/`prune`/`migrate` 핸들러는 SIGTERM 핸들러가
    /// 없어 이 시나리오를 그대로 재현할 수 없다).
    #[tokio::test]
    async fn sigterm_stops_a_lock_holder_and_removes_the_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let profile = "lifecycle-lock-test";
        let lock_path = lock_file_path(dir.path(), profile);

        let mut child = spawn_self_exec_worker(
            "web::job::lifecycle::tests::lifecycle_worker_lock_and_wait_for_term",
            // 프로파일을 argv에도 심는다 — 3축 검증이 명령행에서 찾는다(그 함수 doc 참조).
            // 워커 자신은 여전히 환경변수로 프로파일을 읽는다(락 이름).
            &[profile],
            &[
                ("XB_LIFECYCLE_TEST_LOCK_DIR", dir.path().to_str().unwrap()),
                ("XB_LIFECYCLE_TEST_PROFILE", profile),
            ],
        );
        let pid = child.id().expect("pid 없음");
        let started_at = Utc::now();

        let acquired = wait_until(Duration::from_secs(10), || lock_path.exists()).await;
        assert!(acquired, "워커가 시간 안에 lock을 잡지 못했습니다");

        let identity = JobRecord {
            pid,
            started_at,
            profile: profile.to_string(),
        };
        let outcome = cancel(&identity, Duration::from_secs(10))
            .await
            .expect("cancel 실패");
        assert_eq!(outcome, CancelOutcome::Terminated);

        assert!(
            !lock_path.exists(),
            "SIGTERM을 정상 처리하는 자식인데도 lock 파일이 남아 있다"
        );

        let status = child.wait().await.expect("wait 실패");
        assert!(status.success(), "워커가 정상 반환 경로로 끝나지 않았다");
    }

    // -----------------------------------------------------------------
    // terminate_just_spawned — 배선 도중 실패한 자식 정리
    // -----------------------------------------------------------------

    /// **M1 회귀 방어**: [`terminate_just_spawned`]는 자식을 **회수까지** 한다.
    ///
    /// 이 함수의 존재 이유의 절반이 회수다. 시그널만 보내고 `wait()`를 부르지 않으면 자식이
    /// 좀비로 남아, 애초에 고치려던 문제(배선 실패 → `wait()` 없이 drop → 좀비)가 그대로
    /// 남는다. `waitid(P_PID, ..., WNOWAIT)`로 그 pid를 엿보면 **회수 여부를 정확히** 가를 수
    /// 있다: 좀비면 `Some(true)`, 이미 회수됐으면 커널이 `ECHILD`를 주므로 `None`이다.
    ///
    /// `waitpid(-1, ...)`(아무 자식이나)로 검사하지 않는 이유: 같은 테스트 바이너리에서 동시에
    /// 도는 다른 테스트의 자식을 가로채 회수해 그쪽을 깨뜨린다(실제로 겪었다). pid를 손에 쥔
    /// 곳에서 그 pid만 보는 것이 유일하게 안전한 방법이다.
    ///
    /// 자식은 짧게 끝나는 것을 쓴다 — `JobRunner`는 argv를 자기가 조립하므로(`<verb> ... --json`)
    /// 임의의 장수 프로그램을 띄울 수 없다. SIGTERM→grace→SIGKILL 승격 자체는 [`cancel`]과
    /// **같은 [`send_group_signal`]·[`wait_for_exit`]**를 쓰고 그쪽에 실제 자식으로 도는 테스트가
    /// 있으므로, 여기서는 이 함수 고유의 책임(파이프 닫기 + 회수)에 집중한다.
    #[tokio::test]
    async fn terminate_just_spawned_reaps_the_child() {
        use super::super::spec::{JobCommand, JobSpec};
        use super::super::JobSecrets;

        // `/bin/sleep`을 exe로 주면 `sleep doctor --json`이 되어 즉시 인자 오류로 끝난다 —
        // 여기서 필요한 것은 "실제로 spawn된 우리 자식" 하나뿐이다.
        let runner = super::super::runner::JobRunner::with_exe(
            PathBuf::from("/bin/sleep"),
            None,
            crate::i18n::Lang::En,
            JobSecrets::new(),
        );
        let running = runner
            .spawn(&JobSpec::new(JobCommand::Doctor, Lang::En))
            .expect("spawn 실패");
        let pid = running.handle().pid.expect("pid 없음");

        terminate_just_spawned(running, Duration::from_secs(2))
            .await
            .expect("정리가 에러를 반환하면 안 됨");

        assert!(pid_finished(pid), "정리 후에도 프로세스가 살아 있다");
        assert!(
            waitid_peek(pid).is_none(),
            "회수되지 않은 좀비가 남았다 — `RunningJob::wait()` 계약이 깨졌다"
        );
    }

    // -----------------------------------------------------------------
    // classify_for_reattach
    // -----------------------------------------------------------------

    /// 3중 일치(pid 생존 + 시작 시각 + 명령행의 프로파일 토큰) — 재부착된다.
    ///
    /// `/bin/sleep <N>`을 살아있는 실제 자식으로 쓰되, "명령행에 프로파일 문자열이
    /// 있는가"를 검증하는 대상으로 인자 `N`(예: "77")을 재사용한다 — 프로덕션에서는
    /// `--profile <name>` 인자가 그 자리를 채우지만, 이 테스트가 검증하려는 것은 "실제
    /// 살아있는 프로세스의 명령행을 읽어 토큰이 있는지 판정하는 메커니즘 자체"이므로
    /// sleep의 지속 시간 인자를 그 자리에 대신 세워도 같은 성질을 증명한다.
    #[tokio::test]
    async fn classify_for_reattach_matches_all_three_axes() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("77");
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let started_at = Utc::now();

        let identity = JobRecord {
            pid,
            started_at,
            profile: "77".to_string(),
        };
        assert_eq!(
            classify_for_reattach(&identity),
            ReattachDecision::Reattached
        );

        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 명령행에 없는 프로파일이면(pid·시작 시각은 맞아도) crashed다.
    #[tokio::test]
    async fn classify_for_reattach_rejects_profile_mismatch() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("78");
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let started_at = Utc::now();

        let identity = JobRecord {
            pid,
            started_at,
            profile: "definitely-not-in-argv".to_string(),
        };
        match classify_for_reattach(&identity) {
            ReattachDecision::Crashed(_) => {}
            other => panic!("프로파일 불일치인데 {other:?}가 나왔습니다"),
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 시작 시각이 어긋나면(pid는 살아 있어도) crashed다.
    #[tokio::test]
    async fn classify_for_reattach_rejects_started_at_mismatch() {
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("79");
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");

        let identity = JobRecord {
            pid,
            started_at: Utc::now() - chrono::Duration::hours(2),
            profile: "79".to_string(),
        };
        match classify_for_reattach(&identity) {
            ReattachDecision::Crashed(_) => {}
            other => panic!("시작 시각 불일치인데 {other:?}가 나왔습니다"),
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 죽은 pid는 crashed다.
    #[test]
    fn classify_for_reattach_rejects_dead_pid() {
        let identity = JobRecord {
            pid: u32::MAX - 1,
            started_at: Utc::now(),
            profile: "whatever".to_string(),
        };
        match classify_for_reattach(&identity) {
            ReattachDecision::Crashed(reason) => {
                assert!(reason.contains("존재하지 않습니다"), "{reason}");
            }
            other => panic!("죽은 pid인데 {other:?}가 나왔습니다"),
        }
    }

    // -----------------------------------------------------------------
    // wait_for_orphan_exit
    // -----------------------------------------------------------------

    /// 자연 종료하는 프로세스에 대해, pid가 사라지는 즉시(다음 폴에서) 반환한다.
    #[tokio::test]
    async fn wait_for_orphan_exit_returns_after_natural_exit() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("1")
            .spawn()
            .expect("spawn 실패");
        let pid = child.id().expect("pid 없음");

        let result = tokio::time::timeout(
            Duration::from_secs(5),
            wait_for_orphan_exit(pid, Duration::from_millis(100)),
        )
        .await;
        assert!(
            result.is_ok(),
            "wait_for_orphan_exit이 시간 안에 반환하지 않았다"
        );

        let _ = child.wait().await;
    }

    // -----------------------------------------------------------------
    // OS 원시 도구 자체의 sanity check — 지원되지 않는 환경에서는 건너뛴다
    // (runner.rs의 observer() 관례와 같은 태도).
    // -----------------------------------------------------------------

    /// 방금 띄운 자식의 OS 시작 시각이 우리가 기록한 시각과 허용 오차 안에서 일치한다.
    /// 이 테스트가 실패하면 [`os_process_start_time`] 자체가 이 환경에서 신뢰할 수 없다는
    /// 뜻이므로, [`cancel`]·[`classify_for_reattach`]의 다른 테스트들도 함께 의심해야
    /// 한다.
    #[tokio::test]
    async fn os_process_start_time_matches_freshly_spawned_child() {
        let mut child = tokio::process::Command::new("/bin/sleep")
            .arg("5")
            .spawn()
            .expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let recorded = Utc::now();

        match os_process_start_time(pid) {
            Some(actual) => {
                let diff_ms = (actual - recorded).num_milliseconds().abs();
                assert!(
                    diff_ms <= START_TIME_TOLERANCE_MS,
                    "OS 시작 시각({actual})이 기록된 시각({recorded})과 {diff_ms}ms 차이남"
                );
            }
            None => {
                eprintln!(
                    "건너뜀: 이 환경에서 프로세스 시작 시각을 확인할 수 없습니다\
                     (/proc 미지원 또는 ps 부재)"
                );
            }
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
    }
}
