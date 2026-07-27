//! `GET/POST /backup` — 백업 실행 화면. `GET /backup/{id}` 실행 중 화면·
//! `GET /backup/{id}/events`(SSE)·`POST /backup/{id}/cancel`(취소)까지 함께 배선한다.
//!
//! ## 여기서 처음으로 모든 조각이 실제로 이어진다
//! t10(잡 러너)·t11(스트림 중계 + SSE 허브)·t12(생명주기)·t13(exit 표현)·t14(잡 이력)가
//! 각자 완결된 단위로 만들어 둔 이음매를 이 파일이 실제로 연결한다. 잡을 spawn하는
//! 진짜 진입점은 [`spawn_tracked_job`] 하나뿐이고, `POST /backup`(`submit`)은 그 위에
//! 백업 전용 `JobSpec` 조립·프로파일 검증·중복 실행 방지를 얹은 얇은 층이다.
//!
//! ## backup은 파괴적 명령이 아니다 — 그런데도 감사 로그에 남긴다
//! [`JobCommand::Backup::is_destructive`]는 `false`다(`job::spec` 헤더) — 그래서
//! [`JobRunner::spawn`]("비파괴 잡"만 받는 함수)을 쓰고 `spawn_destructive`는 쓰지 않는다.
//! 그런데도 "누가 언제 무엇을 돌렸나"는 감사 대상이다(리더 지시서). `audit.rs`를 읽은
//! 결론: **비파괴 기록 전용 API는 이미 존재한다 — [`AuditLog::record`]가 그것이다.**
//! [`AuditLog::gate`]는 특별히 파괴적 작업만을 위한 게이트라, `outcome`을 항상
//! [`AuditOutcome::Requested`]로 고정하고 [`AuditReceipt`]를 반환한다. 그 receipt를
//! 아무 데도 쓰지 않고 버리는 것은 컴파일은 되지만 **의미가 없다** — receipt의 존재
//! 이유는 "이 receipt를 `spawn_destructive`에 값으로 넘겨야만 파괴적 작업이 실행된다"는
//! 타입 강제(t8 헤더)이고, 비파괴 명령에는 그 강제를 걸 대상 자체가 없다. 그래서
//! [`spawn_tracked_job`]은 `record()`를 **직접** 두 번 부른다 — spawn 직전에
//! `Requested`(누가·언제·무엇을), 잡이 끝난 뒤 실제 [`JobOutcome`]으로 한 번 더(t10의
//! `audit_outcome()`/`exit_code()`를 그대로 접어, 문자열을 새로 만들지 않는다). 첫 기록이
//! 실패하면(디스크 풀 등) spawn 자체를 막는다 — "기록되지 않은 실행"을 만들지 않기
//! 위해서다.
//!
//! ## 중복 실행 방어 — 두 겹, 경합 창의 크기
//! 같은 프로파일 백업을 두 탭에서 거의 동시에 누르면:
//!
//! 1. **사전 거부(1차).** [`submit`]이 spawn 전에 [`running_job_for_profile`]로
//!    [`JobStore::list`]를 훑어 같은 프로파일의 실행 중 잡이 있는지 본다. 있으면 409와
//!    함께 그 잡을 지켜볼 링크를 준다.
//! 2. **exit 5 표면화(2차, 항상 켜져 있다).** 락은 자식이 잡으므로(모듈 헤더 아래
//!    "파일 락을 재구현하지 마라") 1차 검사와 자식의 실제 락 획득 사이에는 경합 창이
//!    있다. 그 창 안에서 두 요청이 모두 1차 검사를 통과하면, 두 자식이 모두 spawn되고
//!    뒤에 락을 잡는 쪽이 exit 5로 끊긴다. [`JobOutcome::LockConflict`]는 t10/t13/t14
//!    전 구간에서 **실패가 아니라 경고**로 접히므로(`is_success()`는 여전히 false지만
//!    `Level::Warn`, `retryable=true`), 이력에 "실패"가 중복 누적되지 않는다 — 두 겹
//!    중 어느 경로를 타도 운영자가 보는 것은 "성공/도는 중" 아니면 "다른 인스턴스가
//!    잡고 있음"이지, "고쳐야 할 실패"가 아니다.
//!
//! **경합 창의 크기:** [`running_job_for_profile`]의 `await`가 끝나는 시점부터, 자식이
//! 실제로 `flock` 계열 원자적 락 파일 생성(`O_CREAT|O_EXCL`)에 도달하는 시점까지다.
//! 그 사이에 들어가는 것은 (a) 이 함수가 반환한 뒤 [`JobSpec`]을 조립하는 몇 줄의 동기
//! 코드, (b) [`AuditLog::record`]의 `await`(디스크 append 1회), (c)
//! `tokio::process::Command::spawn()`의 OS `fork`+`exec` 비용, (d) 자식이 자기 바이너리를
//! 초기화해 `backup` 핸들러의 락 획득 코드에 도달하기까지의 시간이다. 로컬 디스크
//! 기준으로 전부 합쳐도 **한 자리~두 자리 밀리초** 규모다(감사 로그 append 1회가 가장
//! 크고, 그것도 수백 μs~수 ms). 초 단위로 벌어질 이유가 없으므로 실전에서 "정말 동시에
//! 두 탭에서 버튼을 눌렀을 때"에만 열리는 창이다.
//!
//! ## `spawn_tracked_job` — t35(JobLogSink 연결)가 요구하는 전체 파이프라인
//! stdout/stderr 중계(t11)·잡 로그 적재([`crate::web::state::jobs::JobLogStoreSink`],
//! t35 흡수분)·이력 기록(t14)·SSE 발행(t11)을 한 함수에 모았다. **명령을 가리지
//! 않는다** — `spec.command()`가 `backup`이 아니어도 그대로 동작한다(파괴적 명령만
//! [`JobRunner::spawn`]이 거부한다). 그래서 이 파일의 통합 테스트는 실제 DB가 필요한
//! `backup` 대신, 오프라인으로 도는 `doctor --json`을 이 함수에 흘려보내 spawn → 중계 →
//! 로그 적재 → 이력 기록 → `Done` 이벤트까지 한 번에 관통시킨다(리더 지시서가 명시한
//! 방식).
//!
//! ## `JobEvent::Done`을 어디서 publish하는가
//! t11은 재료만 준비해 뒀다(`relay_stdout`의 [`StdoutRelayResult::summary`]와 자식
//! `wait()`의 [`JobOutcome`]을 합치는 지점은 만들지 않았다 — 그 파일 헤더가 명시적으로
//! 이 태스크에 넘긴다). [`spawn_tracked_job`]의 백그라운드 태스크가 그 지점이다: 두
//! 릴레이 태스크를 나란히 스폰해 두고 [`RunningJob::wait`]로 종료를 기다린 뒤(파이프를
//! 계속 비우는 태스크와 나란히 불러야 교착하지 않는다 — `job::runner` 헤더), stdout
//! 릴레이가 돌려준 요약과 그 종료 상태를 합쳐 **여기서** [`JobEvent::Done`]을 만든다.
//!
//! ## SSE 허브는 [`ServeConfig::live`]가 들고 있다
//! [`crate::web::sse`] 헤더는 "잡 id로 허브를 어떻게 찾는지는 이 파일이 모른다 — 그건 잡
//! 레지스트리의 책임"이라고 적어 뒀지만, t14의 `JobStore`는 **파일 기반 이력**일 뿐 살아
//! 있는 `Arc<JobHub>`를 들고 있지 않다. 그 레지스트리는
//! [`crate::web::state::live::LiveJobs`]이고, 이 파일은 `ctx.live`로 그것을 쓴다.
//!
//! 처음에는 이 파일 안의 프로세스 전역 `static`이었다 — `ServeConfig`가 이 태스크가 손댈
//! 수 없는 파일이라 필드를 얹을 수 없었기 때문이다. 그 우회의 대가(테스트 격리 약화, 다중
//! 인스턴스 불가, 소유 관계가 코드에 안 적힘)와 걷어낸 근거는
//! [`crate::web::state::live`] 헤더에 있다.
//!
//! 완료된 잡의 허브는 [`HUB_RETAIN_AFTER_DONE`] 동안만 남겨 둔다 — 막 도착한 SSE
//! 구독자도 `done` 스냅샷을 재생받을 수 있게 하되([`JobHub::subscribe`]의 스냅샷 재생),
//! 서버 수명 내내 모든 잡의 허브를 붙들어 메모리를 무한정 늘리지 않기 위해서다. 그
//! 유예가 지나 허브가 사라진 뒤에는 `GET /backup/{id}/events`가 404를 낸다 — 완료된
//! 잡의 전체 이력은 [`crate::web::routes::jobs::detail`](`GET /jobs/{id}`)이 항상 갖고
//! 있으므로 정보가 사라지는 것은 아니다.
//!
//! ## 취소는 요청을 붙잡지 않는다
//! [`lifecycle::cancel`]은 SIGTERM을 보내고 [`lifecycle::CANCEL_GRACE_DEFAULT`] 즉 30초 동안
//! 종료를 기다린 뒤 필요하면 SIGKILL로 승격하고 짧게(2초) 더 확인한다. 즉 **최악의 경우
//! 32초가 걸리는 작업**이다.
//!
//! 이전 판본은 그것을 핸들러에서 그대로 `await`했다. 실측 32.2초 — 그 사이 브라우저는 빈
//! 로딩 화면을 물고 있었고, 운영자는 서버가 죽었다고 판단해 새로고침하거나 버튼을 다시
//! 눌렀다. 앞단 프록시의 흔한 `proxy_read_timeout`(nginx 60초, 다른 곳은 30초)에 걸리면
//! 504까지 났다 — 그러면 **취소가 실제로 됐는지조차 화면이 말해주지 못한다.**
//!
//! 30초 grace 자체는 `job::lifecycle` 헤더가 "사람이 기다릴 수 있는 인간 척도"로 근거를
//! 댄 값이고 줄일 이유가 없다. 고쳐야 할 것은 **누가 기다리는가**였다:
//!
//! 1. [`cancel`]이 배지를 `cancelling`으로 발행하고 취소를 [`tokio::spawn`]으로 떼어낸 뒤
//!    **즉시** 303으로 실행 중 화면에 돌려보낸다.
//! 2. [`crate::web::sse::JobHub::subscribe`]가 새 구독자에게 최근 `state`를 재생하므로,
//!    그 화면이 SSE를 새로 열면 곧바로 `cancelling`을 받는다 — 발행이 리다이렉트보다
//!    먼저여야 하는 이유다.
//! 3. 취소가 끝나면 같은 허브로 `Done`이 오고 배지가 결과로 바뀐다.
//!
//! 그래서 운영자는 "누른 것이 먹었다"를 즉시 보고, 결과는 화면이 알아서 갱신한다.
//! 백그라운드로 보내면서 생긴 중복 취소 경합은 아래 "취소 진행 표시"가 막는다.
//!
//! ## 노출하지 않은 옵션 — `--compress-level`
//! 지시서는 "압축 레벨"도 폼에 노출하라고 했지만, [`job::spec`] 헤더가 명시적으로
//! `JobCount` 어휘에서 **뺀** 항목이다(음수 zstd 레벨이 `-`로 시작해 옵션 주입 방어와
//! 정면으로 부딪히고, "압축 레벨은 config가 정하는 것이 정상 경로"라는 설계 결정 —
//! 그 헤더 "정수 값을 받는 옵션의 닫힌 어휘" 참조). 닫힌 어휘를 확장하는 것은 이
//! 태스크의 권한 밖이라 폼에서 뺐다. 필요해지면 `JobCount`에 항목을 추가하는 것부터
//! 시작해야 한다.
//!
//! ## 이 화면이 손대지 않는 것
//! - `crate::engine`·`crate::storage`·`crypto` 직접 호출 없음(최상위 불변식).
//! - 파일 락을 재구현하지 않는다 — 자식이 잡는다.
//! - `crate::web::routes::config`(t27)를 import하지 않는다. 프로파일 목록은
//!   [`load_profile_names`]가 `Config::from_toml_str`을 직접 불러 얻는다 — t27의
//!   `ConfigDocument`는 출처(provenance) 추적까지 계산하는데 이 화면은 이름 목록만
//!   필요해 그 계산이 낭비이고, 병행 작업 중인 파일에 대한 결합도 피한다.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::Markup;

use crate::error::{Result, XBackupError};
use crate::web::audit::{AuditEvent, AuditOutcome};
use crate::web::job::args::ProfileName;
use crate::web::job::stream::{relay_stderr, relay_stdout, JobLogSink};
use crate::web::job::{exit as job_exit, lifecycle, JobCommand, JobFlag, JobOutcome, JobSpec};
use crate::web::mask::SecretRegistry;
use crate::web::sse::{sse_response, JobEvent, JobHub};
use crate::web::state::jobs::{self, JobId, JobLogStoreSink, JobStart, JobStore, LogStream};
use crate::web::view::components::Level;
use crate::web::view::{backup as view, layout};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수 — 라우터(리더)·마크업·테스트가 공유한다
// ---------------------------------------------------------------------------

/// 폼(GET)·제출(POST) 경로.
pub const BACKUP_PATH: &str = "/backup";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const BACKUP_TITLE: &str = "Backup";
/// 실행 중 화면의 **라우터 패턴**(axum 0.8 `{name}` 문법).
pub const BACKUP_RUNNING_ROUTE: &str = "/backup/{job_id}";
/// SSE 스트림의 라우터 패턴.
pub const BACKUP_EVENTS_ROUTE: &str = "/backup/{job_id}/events";
/// 취소 제출(POST)의 라우터 패턴.
pub const BACKUP_CANCEL_ROUTE: &str = "/backup/{job_id}/cancel";

/// 감사 로그 `actor` 필드 값. 이 콘솔에는 세션 토큰 하나만 있고 사용자별 신원이 없으므로
/// (t6), 지금 표현할 수 있는 가장 정직한 값은 "웹에서" 왔다는 사실뿐이다.
const AUDIT_ACTOR: &str = "web";

/// 완료된 잡의 SSE 허브를 이 시간만큼 더 붙들어 둔다(모듈 헤더 "SSE 허브 레지스트리"
/// 참조). 사람이 "방금 끝났다"는 배너를 보고 탭을 닫기까지 여유를 주는 값이다 — 초
/// 단위로 짧게 잡으면 막 도착한 뷰어가 빈 404를 볼 수 있고, 너무 길면 완료된 잡들의
/// 허브가 서버 메모리에 오래 쌓인다.
const HUB_RETAIN_AFTER_DONE: Duration = Duration::from_secs(120);

/// 취소 후 "종료을 아는 쪽"이 이력에 기록을 남기기를 기다리는 폴링 횟수
/// ([`settle_cancelled_job`] 참조).
///
/// 10회 × [`CANCEL_SETTLE_POLL_INTERVAL`](200ms) = 2초. [`lifecycle::cancel`]이 이미 프로세스
/// 종료를 확인한 **뒤**의 대기이므로, 남은 일은 같은 프로세스 안의 태스크가
/// `wait()`에서 깨어나 append 두 번(로그·인덱스)을 하는 것뿐이다 — 로컬 디스크에서 밀리초
/// 규모다. 2초는 그 여유의 수백 배이면서, 고아 잡(기다려도 아무도 오지 않는다)에서 취소
/// 마감이 눈에 띄게 늘어지지 않는 선이다.
///
/// 이 대기는 **백그라운드 취소 태스크 안에서** 일어나므로 HTTP 요청을 붙잡지 않는다
/// (모듈 헤더 "취소는 요청을 붙잡지 않는다") — 운영자가 체감하는 것은 배지가 결과로
/// 바뀌기까지의 시간이고, 2초는 그 화면 갱신에서 눈에 띄지 않는다.
const CANCEL_SETTLE_POLLS: usize = 10;

/// 위 폴링 간격 — [`lifecycle`]의 취소 폴링과 같은 자리수(200ms)를 쓴다. 이력 한 번 읽기는
/// 인덱스 파일 하나를 훑는 비용이므로 이 간격에서 부담이 없다.
const CANCEL_SETTLE_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// 폼 필드 이름.
const FIELD_PROFILE: &str = "profile";
const FIELD_TYPE: &str = "type";
const FIELD_DB: &str = "db";
const FIELD_COLLECTION: &str = "collection";
const FIELD_FROM: &str = "from";
const FIELD_NO_ENCRYPT: &str = "no_encrypt";
const FIELD_NO_HOOKS: &str = "no_hooks";

/// 실행 중 화면 링크.
pub fn backup_running_href(id: &JobId) -> String {
    format!("{BACKUP_PATH}/{id}")
}

/// SSE 스트림 링크.
pub fn backup_events_href(id: &JobId) -> String {
    format!("{BACKUP_PATH}/{id}/events")
}

/// 취소 제출 링크.
pub fn backup_cancel_href(id: &JobId) -> String {
    format!("{BACKUP_PATH}/{id}/cancel")
}

// ---------------------------------------------------------------------------
// 잡 spawn + 배선 — t35가 요구한 전체 파이프라인(모듈 헤더 참조)
// ---------------------------------------------------------------------------

/// 비파괴 잡 하나를 spawn하고, stdout/stderr 중계·로그 적재·이력 기록·SSE 발행까지
/// 전부 배선한 뒤 잡 id를 즉시 돌려준다.
///
/// 반환된 뒤에도 잡은 백그라운드 태스크에서 계속 돈다 — 반환값은 "잡이 시작됐다"는
/// 확인일 뿐 "끝났다"가 아니다. 완료 처리(로그 적재, 이력 기록, `Done` 발행, 허브 정리)는
/// 그 백그라운드 태스크 안에서 이어진다. 명령을 가리지 않는다(모듈 헤더 "왜 명령을
/// 가리지 않는가").
pub async fn spawn_tracked_job(
    ctx: &Arc<ServeConfig>,
    spec: JobSpec,
    actor: &str,
) -> Result<JobId> {
    let actor = actor.to_string();
    let masked_args = spec.masked_args(ctx.jobs.secret_registry());
    let audit_action = spec.audit_action();
    let audit_target = spec.audit_target().to_string();

    // 감사 기록이 실패하면 spawn 자체를 막는다 — "기록되지 않은 실행"을 만들지 않기
    // 위해서다(모듈 헤더 "backup은 파괴적 명령이 아니다" 참조).
    ctx.audit
        .record(AuditEvent {
            actor: &actor,
            action: audit_action,
            target: &audit_target,
            args_masked: &masked_args,
            outcome: AuditOutcome::Requested,
            exit_code: None,
        })
        .await?;

    let mut running = ctx.jobs.spawn(&spec)?;
    let started_at = running.started_at();
    let pid = running.pid();
    let profile = spec.profile().map(|p| p.as_str().to_string());
    let command_verb = spec.command().verb();

    let store = jobs::shared(&ctx.state_dir);
    let job_id = match store
        .start(JobStart {
            command: command_verb,
            profile: profile.as_deref(),
            args_masked: &masked_args,
            started_at,
            pid,
        })
        .await
    {
        Ok(id) => id,
        // **자식은 이미 떠 있다.** 그냥 반환하면 `RunningJob`이 `wait()` 없이 drop되어
        // 락을 든 고아 + 좀비가 남고, 이력에 항목이 없으니 재부착 스캔도 그 잡을 찾지
        // 못한다 — 어떤 회복 경로도 닿지 않는다(`lifecycle::terminate_just_spawned` doc).
        // 그래서 방금 띄운 자식을 정리하고, 그 사실을 감사 로그에 남긴 뒤 실패를 전파한다.
        Err(e) => {
            abandon_untracked_child(
                ctx,
                running,
                &e,
                &actor,
                audit_action,
                &audit_target,
                &masked_args,
            )
            .await;
            return Err(e);
        }
    };

    let hub = Arc::new(JobHub::new(ctx.jobs.secret_registry().clone()));
    // 레벨은 서버가 정해 이벤트에 싣는다 — 스크립트가 자기 마음대로 정하면 서버 렌더와
    // 갈라진다(`view::backup` 헤더 "상태 배지의 레벨은 서버가 정한다"). 도는 잡은 아직
    // 결과가 없으므로 `view::backup::state_badge`의 초기 렌더와 같은 `Ok`를 쓴다.
    hub.publish(JobEvent::State {
        state: view::RUNNING_STATE_LABEL.to_string(),
        level: Level::Ok,
    });
    ctx.live.register_hub(job_id, Arc::clone(&hub));

    // 파이프를 여기서 꺼내 아래 백그라운드 태스크에서 즉시 소비를 시작한다 — 꺼낸 뒤
    // 소비를 미루면(예: 이 함수가 반환할 때까지) 그 사이 자식이 버퍼를 채워 멈출 수
    // 있다(`job::runner` 헤더 "파이프를 비우지 않으면 자식이 멈춘다").
    let stdout = running
        .take_stdout()
        .expect("stdout은 spawn 직후 아무도 꺼내지 않은 상태다");
    let stderr = running
        .take_stderr()
        .expect("stderr은 spawn 직후 아무도 꺼내지 않은 상태다");

    let sink = JobLogStoreSink::spawn(Arc::clone(&store), job_id);
    let sink: Arc<dyn JobLogSink> = Arc::new(sink);

    let lang = ctx.lang;
    let audit = Arc::clone(&ctx.audit);
    // 잡이 끝나면 그 프로파일의 상태 프로브를 낡은 것으로 만든다 — 방금 누른 백업이
    // 대시보드에 반영되지 않으면 운영자는 "실패했나?"를 되묻는다
    // (`crate::web::cache::invalidate_profile` doc).
    let cache_ctx = Arc::clone(ctx);
    let cache_profile = profile.clone();
    // 완료 태스크가 끝날 때 허브를 내린다 — 태스크는 `ctx`보다 오래 살 수 있으므로
    // 참조가 아니라 소유 핸들을 넘긴다.
    let live = Arc::clone(&ctx.live);

    tokio::spawn(async move {
        let stdout_task = tokio::spawn(relay_stdout(stdout, Arc::clone(&hub)));
        let stderr_task = tokio::spawn(relay_stderr(stderr, Arc::clone(&hub), Some(sink)));

        // wait()는 두 릴레이 태스크가 파이프를 계속 비우는 동안 불러야 교착하지 않는다
        // — 위에서 이미 둘 다 별도 태스크로 스폰했으므로 여기서는 그냥 기다리기만
        // 한다(`job::runner` 헤더).
        let wait_result = running.wait().await;

        let stdout_summary = match stdout_task.await {
            Ok(result) => result.summary,
            Err(e) => {
                tracing::warn!(job_id = %job_id, error = %e, "stdout 중계 태스크가 비정상 종료했습니다");
                None
            }
        };
        if let Err(e) = stderr_task.await {
            tracing::warn!(job_id = %job_id, error = %e, "stderr 중계 태스크가 비정상 종료했습니다");
        }

        let outcome = match wait_result {
            Ok(outcome) => outcome,
            Err(e) => {
                tracing::warn!(job_id = %job_id, error = %e, "잡 종료 대기 실패 — Unknown으로 접습니다");
                // JobOutcome에 "종료 대기 자체가 OS 에러로 실패"하는 전용 variant는
                // 없다(t10 어휘 밖) — Unknown이 가장 가까운 뜻("판정 불가")이다.
                JobOutcome::Unknown
            }
        };

        // 운영자용 안내 문구는 t13의 present()로 만든다(문자열 경로를 다시 만들지
        // 않는다 — 지시서 요구사항). 레벨도 여기서 함께 나오므로 SSE `done` 이벤트가
        // 그 값을 그대로 싣는다 — 화면 세 경로(초기 렌더·SSE·이력 화면)가 전부 t13의
        // `level_for_label` 하나를 공유한다.
        let presentation = job_exit::present(&outcome, lang);

        hub.publish(JobEvent::Done {
            outcome: outcome.label().to_string(),
            exit_code: outcome.exit_code(),
            level: presentation.level,
            summary: stdout_summary,
        });

        // 종료 기록이 빠지면 이 잡이 영구히 "실행 중"으로 남아 그 프로파일의 새 백업이
        // 계속 409로 막힌다 — 그래서 한 번의 warn으로 끝내지 않고 재시도까지 하는
        // `finish_persistent`를 쓴다(그 함수 doc 참조).
        if let Err(e) = store.finish_persistent(&job_id, outcome).await {
            tracing::warn!(job_id = %job_id, error = %e, "잡 종료 기록 실패");
        }

        if let Err(e) = store
            .append_log(
                &job_id,
                LogStream::Note,
                &format!("{} {}", presentation.headline, presentation.detail),
            )
            .await
        {
            tracing::warn!(job_id = %job_id, error = %e, "종료 안내 로그 적재 실패");
        }

        // 완료 감사 기록 — outcome/exit_code는 t10 어휘를 그대로 접는다.
        if let Err(e) = audit
            .record(AuditEvent {
                actor: &actor,
                action: audit_action,
                target: &audit_target,
                args_masked: &masked_args,
                outcome: outcome.audit_outcome(),
                exit_code: outcome.exit_code(),
            })
            .await
        {
            tracing::warn!(job_id = %job_id, error = %e, "완료 감사 기록 실패");
        }

        if let Some(profile) = &cache_profile {
            crate::web::cache::invalidate_profile(&cache_ctx, profile);
        }

        tokio::time::sleep(HUB_RETAIN_AFTER_DONE).await;
        live.unregister_hub(&job_id);
    });

    Ok(job_id)
}

/// 이력에 기록되지 못한 자식을 정리하고 감사 로그에 그 사실을 남긴다.
///
/// [`spawn_tracked_job`]의 유일한 호출부다. spawn과 이력 기록 사이에서 실패하면 그 자식은
/// **어떤 관리 경로에도 속하지 않는다** — 이력 항목이 없으니 화면에도, 재부착 스캔에도,
/// 취소 경로에도 나타나지 않는다. 그런 자식을 살려 두는 것은 "락을 든 채 아무도 모르는 백업이
/// 돌고 있다"와 같은 뜻이므로, 여기서 확실히 끝내고 회수한다.
///
/// ## 감사 로그에 반드시 남긴다
/// spawn 직전에 [`AuditOutcome::Requested`]가 이미 기록됐다. 그 줄만 남고 완료 줄이 없으면
/// 감사 로그를 읽는 사람은 "요청은 됐는데 결과를 모른다"로 본다 — 실제로는 "우리가 시작했다가
/// 되돌렸다"이고 그 둘은 다른 사건이다. 그래서 [`AuditOutcome::Failure`]로 한 줄을 더 남긴다.
/// 감사 기록마저 실패하면 경고만 남긴다 — 여기서 실패를 전파해 봐야 호출부가 할 수 있는 일이
/// 없고(자식은 이미 정리됐다), 원래의 실패 원인을 덮어 버린다.
#[allow(clippy::too_many_arguments)] // 감사 항목 한 줄을 만드는 데 필요한 값들이고, 묶으려면
                                     // 이 함수 전용 구조체를 만들어야 하는데 호출부가 하나뿐이라 이득이 없다.
async fn abandon_untracked_child(
    ctx: &Arc<ServeConfig>,
    running: crate::web::job::runner::RunningJob,
    cause: &XBackupError,
    actor: &str,
    audit_action: &'static str,
    audit_target: &str,
    masked_args: &[String],
) {
    let pid = running.handle().pid;
    match lifecycle::terminate_just_spawned(running, lifecycle::CANCEL_GRACE_DEFAULT).await {
        Ok(outcome) => tracing::error!(
            pid = ?pid,
            cause = %cause,
            cleanup = ?outcome,
            "잡 이력 기록에 실패해 방금 띄운 자식을 정리했습니다 — 이 실행은 일어나지 않은 \
             것으로 취급하세요(이력에 항목이 없습니다). state 디렉터리의 디스크 여유·권한을 \
             확인하세요."
        ),
        // 정리 자체가 실패했다 — 자식이 살아 있을 수 있고 이력에는 항목이 없다. 사람이
        // `ps`로 찾아 직접 정리해야 하는 유일한 상황이므로 pid를 로그에 남긴다.
        Err(cleanup_error) => tracing::error!(
            pid = ?pid,
            cause = %cause,
            error = %cleanup_error,
            "잡 이력 기록에 실패한 뒤 자식 정리까지 실패했습니다 — 이 pid가 락을 들고 계속 \
             돌 수 있습니다. `ps`로 확인해 직접 종료하세요."
        ),
    }

    if let Err(e) = ctx
        .audit
        .record(AuditEvent {
            actor,
            action: audit_action,
            target: audit_target,
            args_masked: masked_args,
            outcome: AuditOutcome::Failure,
            exit_code: None,
        })
        .await
    {
        tracing::warn!(error = %e, "이력 기록 실패에 대한 완료 감사 기록도 남기지 못했습니다");
    }
}

// ---------------------------------------------------------------------------
// 프로파일 목록 — config를 직접 읽는다(모듈 헤더 "이 화면이 손대지 않는 것")
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

/// 같은 프로파일의 실행 중 잡이 있으면 그 id를 돌려준다(중복 실행 사전 거부의 1차
/// 방어 — 모듈 헤더 참조).
async fn running_job_for_profile(state_dir: &std::path::Path, profile: &str) -> Option<JobId> {
    let history = JobStore::attach(state_dir).list().await;
    history
        .entries
        .into_iter()
        .find(|entry| entry.is_running() && entry.profile.as_deref() == Some(profile))
        .map(|entry| entry.id)
}

/// 이 요청에서 쓸 시크릿 레지스트리 — `routes::jobs::request_registry`와 같은 판단
/// (러너가 아는 시크릿 + 세션 토큰).
fn request_registry(ctx: &ServeConfig) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    crate::web::mask::register_from_env_names(&mut registry, [crate::web::auth::ENV_WEB_TOKEN]);
    registry
}

// ---------------------------------------------------------------------------
// 폼 본문 파싱 — `crate::web::routes::config`와 같은 이유로 자체 파서를 둔다(그 파일의
// `FormBody`는 비공개라 재사용할 수 없고, 여기 필드는 7개뿐이라 최소 버전으로 충분하다)
// ---------------------------------------------------------------------------

struct FormBody {
    pairs: Vec<(String, String)>,
}

impl FormBody {
    fn parse(body: &str) -> Self {
        let mut pairs = Vec::new();
        for pair in body.as_bytes().split(|&b| b == b'&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(eq) => {
                    let (k, v) = pair.split_at(eq);
                    (percent_decode(k), percent_decode(&v[1..]))
                }
                None => (percent_decode(pair), String::new()),
            };
            pairs.push((name, value));
        }
        Self { pairs }
    }

    fn get(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim())
    }

    fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }
}

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
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

// ---------------------------------------------------------------------------
// 제출 처리
// ---------------------------------------------------------------------------

enum SubmitError {
    /// 검증 실패 — 사용자가 고칠 수 있는 입력 문제.
    Validation(String),
    /// 같은 프로파일이 이미 실행 중이라 사전 거부됨.
    AlreadyRunning(JobId),
    /// 그 외(감사 로그 기록 실패, spawn 실패 등) — 운영자가 서버 쪽을 봐야 한다.
    Internal(String),
}

impl From<XBackupError> for SubmitError {
    fn from(e: XBackupError) -> Self {
        match &e {
            // 값 검증(`args.rs`)·spawn 거부(파괴적 명령 오분류 등)는 전부 Usage로 온다.
            XBackupError::Usage(_) => SubmitError::Validation(e.detail()),
            _ => SubmitError::Internal(e.to_string()),
        }
    }
}

async fn handle_submit(
    ctx: &Arc<ServeConfig>,
    form: &FormBody,
) -> std::result::Result<JobId, SubmitError> {
    let profile_raw = form.get(FIELD_PROFILE).unwrap_or_default();
    let profile = ProfileName::parse(profile_raw, ctx.lang)?;

    // 1차 방어 — 경합 창의 크기는 모듈 헤더 참조.
    if let Some(existing) = running_job_for_profile(&ctx.state_dir, profile.as_str()).await {
        return Err(SubmitError::AlreadyRunning(existing));
    }

    let mut spec = JobSpec::new(JobCommand::Backup, ctx.lang).with_profile(profile);

    let backup_type = match form.get(FIELD_TYPE) {
        Some("incr") => crate::cli::args::BackupType::Incr,
        _ => crate::cli::args::BackupType::Full,
    };
    spec = spec.with_backup_type(backup_type);

    if let Some(db) = form.get(FIELD_DB).filter(|v| !v.is_empty()) {
        spec = spec.with_db(db)?;
    }
    if let Some(collection) = form.get(FIELD_COLLECTION).filter(|v| !v.is_empty()) {
        spec = spec.with_collection(collection)?;
    }
    if let Some(from) = form.get(FIELD_FROM).filter(|v| !v.is_empty()) {
        spec = spec.with_from(from)?;
    }
    if form.flag(FIELD_NO_ENCRYPT) {
        spec = spec.with_flag(JobFlag::NoEncrypt);
    }
    if form.flag(FIELD_NO_HOOKS) {
        spec = spec.with_flag(JobFlag::NoHooks);
    }

    spawn_tracked_job(ctx, spec, AUDIT_ACTOR)
        .await
        .map_err(SubmitError::from)
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /backup` — 실행 폼.
pub async fn form(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let profiles = load_profile_names(&ctx);
    layout::shell(
        ctx.lang,
        BACKUP_TITLE,
        view::form_body(ctx.lang, &profiles, None),
    )
}

/// `POST /backup` — 실행 제출. 성공하면 실행 중 화면으로 303 리다이렉트한다
/// (POST-Redirect-GET — `auth.rs`의 로그인 제출과 같은 패턴, 새로고침이 이중 제출을
/// 만들지 않는다).
pub async fn submit(State(ctx): State<Arc<ServeConfig>>, body: String) -> Response {
    let form_body = FormBody::parse(&body);
    match handle_submit(&ctx, &form_body).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, backup_running_href(&id))],
        )
            .into_response(),
        Err(SubmitError::Validation(message)) => {
            let profiles = load_profile_names(&ctx);
            let notice = view::validation_notice(ctx.lang, &message);
            (
                StatusCode::BAD_REQUEST,
                layout::shell(
                    ctx.lang,
                    BACKUP_TITLE,
                    view::form_body(ctx.lang, &profiles, Some(notice)),
                ),
            )
                .into_response()
        }
        Err(SubmitError::AlreadyRunning(id)) => {
            let profiles = load_profile_names(&ctx);
            let notice = view::already_running_notice(ctx.lang, &backup_running_href(&id));
            (
                StatusCode::CONFLICT,
                layout::shell(
                    ctx.lang,
                    BACKUP_TITLE,
                    view::form_body(ctx.lang, &profiles, Some(notice)),
                ),
            )
                .into_response()
        }
        Err(SubmitError::Internal(message)) => {
            let profiles = load_profile_names(&ctx);
            let notice = view::validation_notice(ctx.lang, &message);
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                layout::shell(
                    ctx.lang,
                    BACKUP_TITLE,
                    view::form_body(ctx.lang, &profiles, Some(notice)),
                ),
            )
                .into_response()
        }
    }
}

/// `GET /backup/{job_id}` — 실행 중(또는 이미 끝난) 잡의 라이브 화면.
pub async fn running(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, BACKUP_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    if detail.summary.is_none() && !detail.log_file_present {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, BACKUP_TITLE, view::unknown_job(ctx.lang)),
        )
            .into_response();
    }
    let registry = request_registry(&ctx);
    let body = view::running_body(ctx.lang, &id, &detail, &registry);
    (StatusCode::OK, layout::shell(ctx.lang, BACKUP_TITLE, body)).into_response()
}

/// `GET /backup/{job_id}/events` — SSE 스트림. 허브가 살아 있을 때만 연다(모듈 헤더
/// "SSE 허브 레지스트리" 참조) — 인증은 상위 `route_layer`가 이미 검사했으므로 여기는
/// 존재 여부만 본다.
pub async fn events(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => return StatusCode::BAD_REQUEST.into_response(),
    };
    match ctx.live.hub_for(&id) {
        Some(hub) => sse_response(hub).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// `POST /backup/{job_id}/cancel` — 취소를 **떼어내 시작하고** 즉시 실행 중 화면으로
/// 되돌린다(모듈 헤더 "취소는 요청을 붙잡지 않는다"). 실제 종료는 백그라운드 태스크가
/// [`lifecycle::cancel`]로 처리하고, 진행·결과는 SSE 배지로 나간다.
pub async fn cancel(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, BACKUP_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    let Some(summary) = detail.summary else {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, BACKUP_TITLE, view::unknown_job(ctx.lang)),
        )
            .into_response();
    };
    if !summary.is_running() {
        // 이미 끝났다 — 취소할 것이 없다(멱등하게 실행 화면으로 되돌린다).
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, backup_running_href(&id))],
        )
            .into_response();
    }
    let (Some(pid), Some(profile)) = (summary.pid, summary.profile.clone()) else {
        return (
            StatusCode::CONFLICT,
            layout::shell(ctx.lang, BACKUP_TITLE, view::cancel_unavailable(ctx.lang)),
        )
            .into_response();
    };
    let identity = lifecycle::JobRecord {
        pid,
        started_at: summary.started_at,
        profile,
    };

    // 이미 취소가 도는 중이면 새로 띄우지 않고 그대로 되돌린다(모듈 헤더 "취소 진행 표시").
    // 화면은 재생된 `cancelling` 배지를 받으므로 두 번째 클릭도 사실을 본다.
    let Some(claim) = ctx.live.claim_cancel(&id) else {
        return (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, backup_running_href(&id))],
        )
            .into_response();
    };

    // 배지를 먼저 `cancelling`으로 바꾼다. **리다이렉트보다 먼저** 발행하는 것이 중요하다 —
    // [`JobHub::subscribe`]가 새 구독자에게 최근 `state`를 재생하므로, 아래 303을 따라간
    // 브라우저가 실행 중 화면에서 SSE를 새로 열면 그 즉시 `cancelling`을 받는다.
    let hub = ctx.live.hub_for(&id);
    if let Some(hub) = &hub {
        hub.publish(JobEvent::State {
            state: view::CANCELLING_STATE_LABEL.to_string(),
            // 아직 결과가 아니지만 "정상 진행 중"도 아니다 — 운영자가 알아야 하는 상태다.
            level: Level::Warn,
        });
    }

    // **취소를 기다리지 않고 응답한다.** 근거는 모듈 헤더 "취소는 요청을 붙잡지 않는다".
    let state_dir = ctx.state_dir.clone();
    let job_id = id;
    tokio::spawn(async move {
        // `claim`을 이 태스크가 소유한다 — 어느 경로로 끝나든 Drop이 표시를 지운다.
        let _claim = claim;
        match lifecycle::cancel(&identity, lifecycle::CANCEL_GRACE_DEFAULT).await {
            Ok(outcome) => {
                settle_cancelled_job(&state_dir, &job_id, &outcome, hub.as_deref()).await
            }
            Err(e) => {
                // 시그널 전송 자체가 OS 에러로 실패했다 — 대상이 살아 있는지도 확신할 수
                // 없으므로 종료 기록을 남기지 않는다(살아 있는 잡을 "끝났다"고 적으면 두 번
                // 실행될 수 있다).
                tracing::warn!(job_id = %job_id, error = %e, "취소 요청 처리 중 오류");
                // 배지를 `cancelling`에 남겨 두면 화면이 거짓말을 한다 — 진행 중이 아니다.
                restore_running_badge(hub.as_deref());
            }
        }
    });

    (
        StatusCode::SEE_OTHER,
        [(header::LOCATION, backup_running_href(&id))],
    )
        .into_response()
}

/// 취소가 프로세스를 끝낸 뒤, **종료 기록이 실제로 남았는지 확인하고 없으면 직접 남긴다.**
///
/// ## 왜 이게 필요한가 — 취소가 stuck을 만들고 있었다
/// 예전 취소 핸들러는 [`lifecycle::cancel`]만 부르고 끝났다. 이 프로세스가 spawn한 잡이라면
/// [`spawn_tracked_job`]의 백그라운드 태스크가 종료를 수거해 기록을 남기지만, **고아 잡에는
/// 그 태스크가 없다**(서버가 재시작하면 사라진다). 그러면 취소는 성공했는데 이력에는 여전히
/// "실행 중"으로 남고, 그 프로파일의 새 백업이 영구히 409로 막힌다 — 운영자가 막힘을 풀려고
/// 누른 취소 버튼이 막힘을 그대로 두는 셈이다.
///
/// ## 왜 곧바로 쓰지 않고 잠깐 기다리는가
/// 이 프로세스가 spawn한 잡이면 그 백그라운드 태스크가 **실제 종료 상태**를 알고 있다
/// ([`RunningJob::wait`]가 돌려준 [`JobOutcome`]). 우리가 아는 것은 "우리가 보낸 시그널"까지이므로
/// 그쪽 기록이 항상 더 정확하다 — 그래서 우선권을 주고 [`CANCEL_SETTLE_POLLS`]회까지 기다린다.
/// 그 안에 기록이 나타나면 아무것도 하지 않는다(종료 레코드를 두 줄 쓰지 않는다).
///
/// ## 무엇으로 마감하는가
/// 우리가 보낸 시그널을 그대로 남긴다([`lifecycle::CancelOutcome::terminating_signal`]) —
/// SIGTERM으로 죽었으면 `Signaled(SIGTERM)`, grace를 넘겨 SIGKILL로 승격했으면
/// `Signaled(SIGKILL)`다. 이게 정확한 이유: 취소로 죽은 잡의 결과는 실패가 아니라 "밖에서
/// 끊겼다"이고, t13의 표현 계층이 그 구분을 이미 문구로 들고 있다("산출물이 반쯤 남아 있을 수
/// 있습니다").
///
/// [`lifecycle::CancelOutcome::Refused`]는 **아무것도 쓰지 않는다** — 신원 확인에 실패해
/// 시그널을 보내지 않았다는 뜻이고, 대상은 여전히 살아 있을 수 있다. 여기서 마감하면 도는
/// 잡을 끝났다고 기록해 이중 실행의 문을 연다. [`lifecycle::CancelOutcome::AlreadyExited`]는
/// 시그널을 보내지 않았지만 프로세스가 확실히 없으므로 [`JobOutcome::Unknown`]으로 마감한다
/// (어떻게 끝났는지는 알 수 없다 — 추측하지 않는다).
/// `hub`가 있으면 화면 상태도 함께 정리한다 — 배지를 `cancelling`에 남겨 두면 취소가 끝난
/// 뒤에도 화면이 "취소 중"이라고 거짓말한다(M5에서 그 상태를 도입했으므로 그 출구도 함께
/// 만든다).
async fn settle_cancelled_job(
    state_dir: &std::path::Path,
    id: &JobId,
    outcome: &lifecycle::CancelOutcome,
    hub: Option<&JobHub>,
) {
    if let lifecycle::CancelOutcome::Refused(reason) = outcome {
        tracing::warn!(
            job_id = %id,
            reason = %reason,
            "취소를 거부했습니다(신원 확인 실패 또는 시그널 전송 거부) — 대상이 여전히 살아 \
             있을 수 있으므로 종료 기록을 남기지 않습니다"
        );
        // 잡은 계속 돈다 — 배지를 되돌려 화면이 사실을 말하게 한다.
        restore_running_badge(hub);
        return;
    }

    let store = jobs::shared(state_dir);
    for _ in 0..CANCEL_SETTLE_POLLS {
        if !is_still_running(&store, id).await {
            // 종료를 아는 쪽이 이미 기록했다 — 그쪽이 `Done`도 발행했으므로 화면도 이미 맞다.
            return;
        }
        tokio::time::sleep(CANCEL_SETTLE_POLL_INTERVAL).await;
    }

    let recorded = match outcome.terminating_signal() {
        Some(signal) => JobOutcome::Signaled(signal),
        None => JobOutcome::Unknown,
    };
    tracing::info!(
        job_id = %id,
        outcome = recorded.label(),
        "취소된 잡의 종료 기록이 없어 직접 남깁니다(이 서버가 spawn한 잡이 아니거나 완료 \
         처리가 이미 사라졌습니다)"
    );
    if let Err(e) = store.finish_persistent(id, recorded).await {
        tracing::warn!(job_id = %id, error = %e, "취소된 잡의 종료 기록 실패");
    }
    // 우리가 종료를 기록한 쪽이므로 `Done`도 우리가 발행한다 — 안 하면 배지가 `cancelling`에
    // 멈춘 채로 남는다. 레벨은 다른 모든 경로와 같은 함수에서 나온다.
    if let Some(hub) = hub {
        hub.publish(JobEvent::Done {
            outcome: recorded.label().to_string(),
            exit_code: recorded.exit_code(),
            level: job_exit::level_for_label(recorded.label()),
            // 취소된 잡의 최종 요약 문서는 없다(자식이 그것을 찍기 전에 끊겼다).
            summary: None,
        });
    }
}

/// 상태 배지를 다시 `running`으로 되돌린다 — 취소가 이뤄지지 않았을 때 쓴다.
///
/// 허브가 없으면(완료 유예가 지났거나 이 서버가 spawn하지 않은 잡) 할 일이 없다. 그 경우
/// 라이브 화면은 애초에 SSE에 붙지 못하고 서버 렌더 스냅샷만 보므로, 새로고침하면 이력이
/// 말하는 사실(`running`)이 그대로 보인다.
fn restore_running_badge(hub: Option<&JobHub>) {
    if let Some(hub) = hub {
        hub.publish(JobEvent::State {
            state: view::RUNNING_STATE_LABEL.to_string(),
            level: Level::Ok,
        });
    }
}

/// 그 잡이 이력에서 아직 "실행 중"으로 보이는지. 모르는 id는 "아니다"로 본다(마감할 대상이
/// 없다).
async fn is_still_running(store: &JobStore, id: &JobId) -> bool {
    store
        .list()
        .await
        .entries
        .iter()
        .find(|entry| entry.id == *id)
        .is_some_and(jobs::JobSummary::is_running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::web::auth::{self, AuthState};
    use axum::body::Body;
    use axum::http::{header as http_header, Request};
    use axum::middleware;
    use axum::routing::{get, post};
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token-backup-7c1e";

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

    /// 이 화면 넷만 담은 최소 라우터(`routes::jobs`의 테스트 패턴과 동일 — 리더가 붙일
    /// 모양 그대로 여기서 조립한다).
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(BACKUP_PATH, get(form).post(submit))
            .route(BACKUP_RUNNING_ROUTE, get(running))
            .route(BACKUP_EVENTS_ROUTE, get(events))
            .route(BACKUP_CANCEL_ROUTE, post(cancel))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .with_state(ctx)
    }

    async fn get_req(
        ctx: Arc<ServeConfig>,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri);
        if let Some(token) = cookie {
            builder = builder.header(
                http_header::COOKIE,
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

    async fn post_req(
        ctx: Arc<ServeConfig>,
        uri: &str,
        body: &str,
        cookie: Option<&str>,
    ) -> Response {
        let mut builder = Request::builder().method("POST").uri(uri).header(
            http_header::CONTENT_TYPE,
            "application/x-www-form-urlencoded",
        );
        if let Some(token) = cookie {
            builder = builder.header(
                http_header::COOKIE,
                format!("{}={token}", auth::SESSION_COOKIE_NAME),
            );
        }
        router(ctx)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .expect("라우터 호출 실패")
    }

    /// 백그라운드 취소 태스크가 끝날 때까지 기다린다.
    ///
    /// `POST .../cancel`은 취소를 기다리지 않고 303을 돌려주므로(모듈 헤더 "취소는 요청을
    /// 붙잡지 않는다"), 취소의 **결과**를 단정하는 테스트는 응답만 받고 확인해서는 안 된다.
    ///
    /// 완료 판정은 취소 진행 표시가 지워졌는지로 한다 — [`CancelClaim`]을 그 태스크가
    /// 소유하므로 표시가 사라진 시점이 곧 태스크가 끝난 시점이다. 잡 상태를 폴링하는 것보다
    /// 정확하다: `Refused` 경로는 잡을 끝내지 않으므로 상태만 보면 영원히 기다린다.
    async fn await_cancel_settled(ctx: &ServeConfig, id: &JobId) {
        for _ in 0..600 {
            if !ctx.live.cancel_in_flight(id) {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        panic!("백그라운드 취소 태스크가 60초 안에 끝나지 않았다");
    }

    /// 취소 응답 지연 테스트가 쓰는 프로파일 이름 — 자식 argv에 심어 3축 검증을 통과시킨다.
    #[cfg(unix)]
    const SLOW_CANCEL_PROFILE: &str = "backup-slow-cancel";

    /// SIGTERM을 무시하는 자식을 띄운다 — SIGKILL만이 끝낼 수 있다.
    ///
    /// `crate::web::job::lifecycle`의 테스트 워커를 재실행한다(같은 테스트 바이너리이므로
    /// 이름으로 부를 수 있다). 그 파일의 `spawn_self_exec_worker`는 자기 테스트 모듈에
    /// 갇혀 있어 여기서 쓸 수 없으므로 같은 방식만 옮겨 온다 — 프로파일을 **argv 토큰으로**
    /// 심는 것이 핵심이다(취소의 3축 검증이 `/proc/<pid>/cmdline`에서 찾으므로 env로는 안 된다).
    #[cfg(unix)]
    fn spawn_ignore_term_worker(profile: &str) -> tokio::process::Child {
        let exe = std::env::current_exe().expect("테스트 바이너리 경로를 얻지 못했습니다");
        let mut cmd = tokio::process::Command::new(exe);
        cmd.arg("web::job::lifecycle::tests::lifecycle_worker_ignore_term_forever");
        cmd.arg(profile);
        cmd.arg("--exact");
        cmd.env("XB_LIFECYCLE_TEST_IGNORE_TERM", "1");
        cmd.process_group(0);
        cmd.spawn().expect("워커 spawn 실패")
    }

    /// 테스트 config 파일(프로파일 하나) + 그 경로를 문 [`ServeConfig`]를 만든다.
    fn ctx_with_profile(profile: &str) -> (Arc<ServeConfig>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        std::fs::write(
            &config_path,
            format!(
                "[profiles.{profile}.source]\nuri_env = \"XB_TEST_{profile_upper}_URI\"\n\n[profiles.{profile}.destination]\ntype = \"local\"\npath = \"/tmp/{profile}\"\n",
                profile_upper = profile.to_uppercase(),
            ),
        )
        .unwrap();
        let mut cfg = ServeConfig::for_test();
        cfg.config_path = Some(config_path);
        (Arc::new(cfg), dir)
    }

    // ---- 인증 ----

    /// 폼은 인증 없이는 401이고, 쿠키가 있으면 200이다.
    #[tokio::test]
    async fn form_requires_auth_then_renders() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let (status, body) = get_req(Arc::clone(&ctx), BACKUP_PATH, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert!(!body.contains(BACKUP_TITLE), "미인증 응답이 화면을 흘렸다");

        let (status, body) = get_req(ctx, BACKUP_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("prod"), "프로파일 옵션 누락: {body}");
        assert!(body.contains(r#"name="no_encrypt""#));
    }

    // ---- argv 조립(셸 비경유) ----

    /// 값 검증을 통과한 제출이 정확한 `JobSpec`(argv)을 만든다 — 셸을 전혀 거치지 않고
    /// (`job::runner` 헤더의 성질을 그대로 물려받는다) `--type incr`·`--db`·
    /// `--collection`이 정확히 실린다.
    #[test]
    fn valid_submission_builds_expected_argv() {
        let mut spec = JobSpec::new(JobCommand::Backup, Lang::En)
            .with_profile(ProfileName::parse("prod", Lang::En).unwrap())
            .with_backup_type(crate::cli::args::BackupType::Incr);
        spec = spec.with_db("app").unwrap();
        spec = spec.with_collection("users").unwrap();
        spec = spec.with_flag(JobFlag::NoEncrypt);
        spec = spec.with_flag(JobFlag::NoHooks);
        let argv = spec.to_argv();
        assert_eq!(argv[0], "backup");
        for expected in [
            "--profile",
            "prod",
            "--type",
            "incr",
            "--db",
            "app",
            "--collection",
            "users",
            "--no-encrypt",
            "--no-hooks",
        ] {
            assert!(
                argv.iter().any(|a| a == expected),
                "{expected} 누락: {argv:?}"
            );
        }
        assert_eq!(argv.last().unwrap(), "--json");
    }

    // ---- 검증 실패가 실행 전 거부된다 ----

    /// 잘못된 프로파일명은 spawn 이전에 거부된다(자식이 뜨지 않는다).
    #[tokio::test]
    async fn invalid_profile_is_rejected_before_spawn() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let response = post_req(
            ctx,
            BACKUP_PATH,
            "profile=--force&type=full",
            Some(session()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Could not start backup"), "{body}");
    }

    /// `collection`은 통과하지만 `db`가 옵션 주입 형태면 거부된다 — 값 검증이 실제로
    /// `args.rs`를 거친다는 증거(재구현이 아니다).
    #[tokio::test]
    async fn hostile_db_value_is_rejected() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let response = post_req(
            ctx,
            BACKUP_PATH,
            "profile=prod&type=full&db=--force",
            Some(session()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }

    /// 적대적 입력(널 바이트·HTML 태그·제어문자)이 폼 경로를 지나도 거부된다.
    #[tokio::test]
    async fn adversarial_field_values_are_rejected() {
        let (ctx, _dir) = ctx_with_profile("prod");
        for body in [
            "profile=prod&type=full&db=%3Cscript%3E",
            "profile=prod&type=full&db=a%00b",
            "profile=prod&type=full&from=-f",
        ] {
            let response = post_req(Arc::clone(&ctx), BACKUP_PATH, body, Some(session())).await;
            assert_eq!(
                response.status(),
                StatusCode::BAD_REQUEST,
                "'{body}'가 통과했다"
            );
        }
    }

    // ---- 중복 실행 방어 ----

    /// 이력에 같은 프로파일의 실행 중 잡을 심어 두면, 새 제출이 spawn 없이 409로
    /// 사전 거부된다.
    #[tokio::test]
    async fn duplicate_profile_run_is_rejected_before_spawn() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);
        let running_id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[
                    "backup".to_string(),
                    "--profile".to_string(),
                    "prod".to_string(),
                ],
                started_at: chrono::Utc::now(),
                pid: Some(999_999),
            })
            .await
            .unwrap();
        // 종료 기록을 남기지 않으므로 이 잡은 "실행 중"으로 보인다.

        let response = post_req(
            Arc::clone(&ctx),
            BACKUP_PATH,
            "profile=prod&type=full",
            Some(session()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::CONFLICT);
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8_lossy(&bytes);
        assert!(body.contains("Already running"), "{body}");
        assert!(
            body.contains(&backup_running_href(&running_id)),
            "실행 중인 잡 링크 누락: {body}"
        );

        // 이력에 새 잡이 추가되지 않았다(사전 거부는 spawn 자체를 하지 않는다).
        let history = JobStore::attach(&ctx.state_dir).list().await;
        assert_eq!(history.entries.len(), 1, "사전 거부됐는데 잡이 추가됐다");
    }

    // ---- exit 표시 ----

    /// exit 0/4/5가 화면에서 서로 다른 배지 레벨로 표시되고, 4·5는 실패(`fail`)로
    /// 표시되지 않는다 — t13의 [`job_exit::level_for_label`]을 부르므로 이력 화면과 색이
    /// 어긋나지 않는다(예전에는 `view::jobs`가 자기 사본을 들고 있어 실제로 어긋났다).
    #[test]
    fn exit_zero_four_five_render_distinct_non_fail_levels() {
        let ok = job_exit::level_for_label(JobOutcome::Succeeded.label());
        let warn4 = job_exit::level_for_label(JobOutcome::SucceededWithWarnings.label());
        let warn5 = job_exit::level_for_label(JobOutcome::LockConflict.label());
        assert_ne!(ok, warn4);
        assert_ne!(ok, warn5);
        for level in [ok, warn4, warn5] {
            assert_ne!(level, Level::Fail);
        }
    }

    /// SSE `done` 이벤트가 싣는 레벨이 **서버 렌더와 같은 함수**에서 나온다.
    ///
    /// 이 파일이 `Done`을 만들 때 쓰는 값은 [`job_exit::present`]의 `level`이고, 초기 렌더
    /// (`view::backup::state_badge`)와 이력 화면은 [`job_exit::level_for_label`]을 쓴다. 둘이
    /// 갈라지면 같은 잡이 새로고침 전후로 다른 색이 된다 — 그게 예전의 결함이었다(스크립트가
    /// 종료 시 무조건 `warn`을 박아 exit 0 성공이 경고로 떴다).
    #[test]
    fn done_event_level_matches_the_server_rendered_level() {
        for outcome in [
            JobOutcome::Succeeded,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::Failed,
            JobOutcome::Rejected,
            JobOutcome::PrecheckFailed,
            JobOutcome::LockConflict,
            JobOutcome::UnexpectedExit(42),
            JobOutcome::Signaled(15),
            JobOutcome::Unknown,
        ] {
            // 실제 publish 경로는 `ctx.lang`을 쓴다 — 레벨은 언어와 무관해야 하므로
            // 양쪽 언어에서 같은 값이 나오는지도 함께 본다.
            for lang in [crate::i18n::Lang::En, crate::i18n::Lang::Ko] {
                assert_eq!(
                    job_exit::present(&outcome, lang).level,
                    job_exit::level_for_label(outcome.label()),
                    "{outcome:?}({lang:?})의 SSE 레벨이 서버 렌더 레벨과 다르다"
                );
            }
        }
    }

    // ---- 취소 ----

    /// 이미 끝난 잡에 취소를 보내면(멱등) 실행 화면으로 그대로 돌아간다 — 실제 신호를
    /// 보내지 않는다(`lifecycle::cancel`이 애초에 불리지 않는다, `is_running()`이 먼저
    /// 걸러낸다).
    #[tokio::test]
    async fn cancel_on_finished_job_is_a_no_op_redirect() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: Some(4242),
            })
            .await
            .unwrap();
        store.finish(&id, JobOutcome::Succeeded).await.unwrap();

        let response = post_req(ctx, &backup_cancel_href(&id), "", Some(session())).await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
    }

    /// **l15가 고친 성질**: SIGTERM을 무시하는 자식이 있어도 취소 응답은 즉시 온다.
    ///
    /// 예전에는 핸들러가 `lifecycle::cancel`을 그대로 await했다. 대상이 SIGTERM을 무시하면
    /// grace 30초 + SIGKILL 확인 2초를 **브라우저가 매달려서** 기다렸고, 운영자는 서버가
    /// 죽었다고 판단해 새로고침하거나 버튼을 다시 눌렀다. 앞단 프록시의
    /// `proxy_read_timeout`이 30초면 504까지 났다.
    ///
    /// 이 테스트가 재는 것은 **응답 지연 하나**다. 취소가 실제로 프로세스를 끝내는지는
    /// `lifecycle`의 `cancel_escalates_to_sigkill_after_grace_timeout`이 이미 고정한다.
    /// 자식이 3축 검증을 통과해야 `cancel`이 grace 대기까지 들어가므로(거부되면 즉시
    /// 돌아와 이 테스트가 아무것도 증명하지 못한다) 프로파일을 argv에 심는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_responds_immediately_even_when_the_child_ignores_sigterm() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);

        let mut child = spawn_ignore_term_worker(SLOW_CANCEL_PROFILE);
        let pid = child.id().expect("pid 없음");
        // 워커가 signal(SIG_IGN)을 설치할 시간을 준다(느린 CI 감안해 넉넉히).
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some(SLOW_CANCEL_PROFILE),
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: Some(pid),
            })
            .await
            .unwrap();

        let started = std::time::Instant::now();
        let response = post_req(
            Arc::clone(&ctx),
            &backup_cancel_href(&id),
            "",
            Some(session()),
        )
        .await;
        let elapsed = started.elapsed();

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        // 동기 시절이면 여기서 최소 30초가 걸렸다. 2초는 그 둘을 확실히 가르는 선이면서
        // 느린 CI의 파일 I/O 흔들림에는 걸리지 않는 여유다.
        assert!(
            elapsed < std::time::Duration::from_secs(2),
            "취소 응답이 {elapsed:?} 걸렸다 — 요청이 취소를 기다리고 있다"
        );

        // **이 단정이 위의 지연 측정을 의미 있게 만든다.** 취소가 어떤 이유로든 즉시
        // 끝났다면 빠른 응답은 이 변경의 성과가 아니다 — 동기 시절에도 빨랐을 테니까.
        // 취소 진행 표시가 아직 잡혀 있다는 것은 응답이 **끝나지 않은 취소를 두고 먼저
        // 돌아왔다**는 뜻이고, 그게 이 태스크가 고친 성질 그 자체다.
        //
        // 자식이 SIGTERM을 무시하고 grace가 30초이므로 이 창은 넉넉하다.
        assert!(
            ctx.live.cancel_in_flight(&id),
            "응답이 돌아온 시점에 취소가 이미 끝났다 — 이 측정은 아무것도 증명하지 않는다"
        );

        // 정리: 자식을 지워 백그라운드 태스크가 grace 30초를 다 기다리지 않게 한다.
        //
        // 여기서 취소의 **결과**는 단정하지 않는다. 지금 자식을 죽이면 그 태스크가 좀비를
        // 보게 될 수 있고(좀비는 argv가 비어 있어 3축 검증이 거부한다), 그건 이 테스트의
        // 관심사가 아니다. 취소가 실제로 프로세스를 끝내는 경로는
        // `lifecycle::tests::cancel_escalates_to_sigkill_after_grace_timeout`이 고정한다.
        child.start_kill().expect("자식 SIGKILL 실패");
        let _ = child.wait().await;
        await_cancel_settled(&ctx, &id).await;
    }

    // 취소 진행 표시(중복 취소 차단·Drop 해제·인스턴스 간 격리)의 성질은
    // `crate::web::state::live`의 단위 테스트가 고정한다 — 그 타입의 책임이므로 그 옆에
    // 둔다. 이 파일이 확인할 것은 "핸들러가 그것을 실제로 쓴다"이고, 위
    // `cancel_responds_immediately_even_when_the_child_ignores_sigterm`의 in-flight 단정이
    // 그 배선을 본다.

    /// **H1**: 취소가 종료 기록을 채운다 — 취소했는데 이력에 "실행 중"이 남으면 그
    /// 프로파일의 새 백업이 계속 409로 막힌다.
    ///
    /// 이 서버가 spawn하지 않은 잡(= 고아)을 흉내내기 위해 존재하지 않는 pid를 심는다.
    /// [`lifecycle::cancel`]은 `AlreadyExited`를 돌려주고(시그널을 보내지 않는다),
    /// [`settle_cancelled_job`]이 아무도 기록을 남기지 않는 것을 확인한 뒤 직접 마감해야
    /// 한다. 프로파일 이름을 argv 토큰으로도 심을 필요는 없다 — pid가 죽어 있으면 3축 검증이
    /// 1축에서 끝난다.
    #[tokio::test]
    async fn cancel_records_the_end_for_a_job_nobody_owns() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: Some(u32::MAX - 1),
            })
            .await
            .unwrap();
        assert!(
            is_still_running(&store, &id).await,
            "테스트 전제: 취소 전에는 실행 중으로 보인다"
        );

        let response = post_req(
            Arc::clone(&ctx),
            &backup_cancel_href(&id),
            "",
            Some(session()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // 응답은 취소를 기다리지 않으므로 여기서 결과를 보려면 태스크를 기다려야 한다.
        await_cancel_settled(&ctx, &id).await;

        assert!(
            !is_still_running(&store, &id).await,
            "취소 후에도 실행 중으로 남았다 — 그 프로파일의 백업이 영구히 409로 막힌다"
        );
        let summary = store
            .list()
            .await
            .entries
            .into_iter()
            .find(|entry| entry.id == id)
            .expect("이력 항목 누락");
        assert!(summary.finished_at.is_some(), "finished_at이 비어 있다");
        // pid가 이미 없었으므로 우리는 시그널을 보내지 않았다 — 어떻게 끝났는지 모른다.
        assert_eq!(
            summary.outcome.as_deref(),
            Some(JobOutcome::Unknown.label()),
            "보내지 않은 시그널을 기록하면 안 된다"
        );
    }

    /// **H1**: 재부착 스캔이 stuck 항목을 마감하면 같은 프로파일 백업이 다시 통과한다.
    ///
    /// 재현하려는 결함: 서버가 잡이 도는 중 `kill -9`로 죽으면 인덱스에 `start`만 남고
    /// `end`가 없어 그 프로파일이 **영구히** 409로 막혔다. 기동 시 스캔이 그 항목을 마감하면
    /// 다음 제출이 통과해야 한다.
    ///
    /// 마지막 제출은 실제로 자식을 띄운다(`doctor_job_flows_through_relay_log_and_history`와
    /// 같은 방식으로, 테스트 바이너리 자신이 자식이 된다) — 여기서 확인하는 것은 "409가 아니라
    /// 진행됐다"는 것뿐이므로 그 자식이 어떻게 끝나는지는 관심사가 아니다.
    #[tokio::test]
    async fn reattach_scan_unblocks_a_profile_stuck_by_a_dead_server() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);
        store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &["backup".to_string()],
                started_at: chrono::Utc::now(),
                pid: Some(u32::MAX - 1),
            })
            .await
            .unwrap();

        // (0) 스캔 전에는 막혀 있다 — 이것이 고치려는 결함이다.
        let blocked = post_req(
            Arc::clone(&ctx),
            BACKUP_PATH,
            "profile=prod&type=full",
            Some(session()),
        )
        .await;
        assert_eq!(
            blocked.status(),
            StatusCode::CONFLICT,
            "테스트 전제: 스캔 전에는 409로 막힌다"
        );

        // (a) 스캔이 3축 불일치 항목을 마감한다.
        let report = crate::web::reattach::scan(&store, ctx.lang).await;
        assert_eq!(report.closed, 1, "{report:?}");

        // (b) 이제 같은 프로파일 백업이 409가 아니다.
        let response = post_req(
            Arc::clone(&ctx),
            BACKUP_PATH,
            "profile=prod&type=full",
            Some(session()),
        )
        .await;
        assert_eq!(
            response.status(),
            StatusCode::SEE_OTHER,
            "스캔이 마감했는데도 여전히 막혀 있다"
        );
    }

    /// **M1**: 이력 기록이 실패하면 방금 띄운 자식을 살려 두지 않는다.
    ///
    /// 재현: 잡 이력 디렉터리를 **읽기 전용**으로 만들어 `JobStore::start`의 append를 실패시킨다.
    /// 그 시점에는 자식이 이미 spawn돼 있다 — 예전 코드는 `?`로 그냥 반환해 `RunningJob`이
    /// `wait()` 없이 drop됐고, 그러면 자식이 **락을 든 채 고아**로 남고(다음 백업이 exit 5로
    /// 막힌다) 이후 좀비가 된다. 이력에 항목이 없으니 재부착 스캔도 그 잡을 찾지 못한다.
    ///
    /// 이 테스트가 확인하는 것은 **배선**이다: 실패가 전파되고, 이력에 항목이 남지 않고,
    /// 그 사이에 정리 경로가 실제로 불렸는지(패닉·행 없이 반환). "자식이 정말 죽고 회수됐는가"는
    /// pid를 손에 쥔 곳에서 봐야 정확하므로 [`lifecycle::terminate_just_spawned`]의 단위
    /// 테스트가 맡는다 — 여기서 `waitpid(-1, ...)` 같은 프로세스 전역 검사를 쓰면 같은 테스트
    /// 바이너리에서 동시에 도는 **다른 테스트의 자식을 가로채 회수해** 그쪽을 깨뜨린다
    /// (실제로 겪었다).
    #[tokio::test]
    async fn history_write_failure_does_not_leave_the_child_running() {
        let ctx = Arc::new(ServeConfig::for_test());
        // 잡 이력 디렉터리를 만들되 쓰기를 막는다. `JobStore::open`이 디렉터리를 만든 뒤
        // 권한을 조여야 하므로 순서가 중요하다.
        let store = jobs::shared(&ctx.state_dir);
        let jobs_dir = store.dir().to_path_buf();
        std::fs::create_dir_all(&jobs_dir).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&jobs_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
        }

        // `doctor`를 쓴다 — 오프라인으로 도는 잡이고, 이 테스트의 관심사는 자식이 무엇을
        // 하는지가 아니라 "spawn된 자식이 정리되는가"다(t35 통합 테스트와 같은 대역).
        let spec = JobSpec::new(JobCommand::Doctor, Lang::En);
        let before = store.list().await.entries.len();

        let result = spawn_tracked_job(&ctx, spec, "test").await;

        // 권한을 되돌려 놓는다(뒤의 단정이 이력을 읽어야 하고, 임시 디렉터리 정리도 막힌다).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&jobs_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        assert!(
            result.is_err(),
            "이력 기록이 실패했는데 성공을 돌려줬다 — 추적되지 않는 잡이 생긴다"
        );
        assert_eq!(
            store.list().await.entries.len(),
            before,
            "기록이 실패했는데 항목이 생겼다"
        );
    }

    /// **M5**: 취소를 시작하면 `cancelling` 상태를 SSE로 발행한다 — 취소가 백그라운드에서
    /// 도는 동안(최대 ~32초) 화면이 `running`에 멈춰 있지 않게 한다.
    ///
    /// 취소가 거부되면(대상이 우리 잡이 아니다) 배지를 `running`으로 되돌리는 것까지 함께
    /// 본다 — `cancelling`에 남겨 두면 화면이 "취소 중"이라고 거짓말한다.
    #[tokio::test]
    async fn cancel_publishes_cancelling_then_restores_on_refusal() {
        use futures::StreamExt;

        let (ctx, _dir) = ctx_with_profile("prod");
        let store = jobs::shared(&ctx.state_dir);
        // 살아 있지만 **우리 잡이 아닌** pid를 심는다 — 3축 검증이 거부하므로 취소가
        // `Refused`로 끝나고, 그 경로의 배지 복구까지 관측할 수 있다.
        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg("31");
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: Some(pid),
            })
            .await
            .unwrap();

        // 허브를 등록해 SSE 발행을 관측할 수 있게 한다(실제 spawn 경로가 하는 것과 같다).
        let hub = Arc::new(JobHub::new(SecretRegistry::new()));
        ctx.live.register_hub(id, Arc::clone(&hub));
        let mut stream = Box::pin(hub.subscribe());

        let response = post_req(
            Arc::clone(&ctx),
            &backup_cancel_href(&id),
            "",
            Some(session()),
        )
        .await;
        assert_eq!(response.status(), StatusCode::SEE_OTHER);

        // 발행된 상태 전이를 순서대로 읽는다.
        let mut states = Vec::new();
        while let Some(event) = stream.next().await {
            if let JobEvent::State { state, .. } = event {
                states.push(state);
                if states.len() == 2 {
                    break;
                }
            }
        }
        assert_eq!(
            states,
            vec![
                view::CANCELLING_STATE_LABEL.to_string(),
                view::RUNNING_STATE_LABEL.to_string()
            ],
            "취소 중 표시 → (거부됐으므로) 실행 중 복구 순서여야 한다"
        );

        ctx.live.unregister_hub(&id);
        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 모르는 잡에 취소를 보내면 404다.
    #[tokio::test]
    async fn cancel_on_unknown_job_is_not_found() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let missing = JobId::generate();
        let response = post_req(ctx, &backup_cancel_href(&missing), "", Some(session())).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    // ---- SSE ----

    /// 허브가 등록되어 있지 않은 잡 id의 이벤트 스트림은 404다(완료 유예 시간이 지났거나
    ///애초에 이 서버가 spawn하지 않은 잡).
    #[tokio::test]
    async fn events_for_unregistered_job_is_not_found() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let missing = JobId::generate();
        let (status, _) = get_req(ctx, &backup_events_href(&missing), Some(session())).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    // ---- 마크업 안전성 ----

    /// 실행 중 화면·폼 어디에도 색 리터럴·인라인 스타일이 없다.
    #[tokio::test]
    async fn markup_has_no_color_or_inline_style() {
        let (ctx, _dir) = ctx_with_profile("prod");
        let (_, form_body) = get_req(Arc::clone(&ctx), BACKUP_PATH, Some(session())).await;
        assert!(!form_body.contains("style="));

        let store = jobs::shared(&ctx.state_dir);
        let id = store
            .start(JobStart {
                command: "backup",
                profile: Some("prod"),
                args_masked: &[],
                started_at: chrono::Utc::now(),
                pid: Some(4242),
            })
            .await
            .unwrap();
        let (_, running_body) = get_req(ctx, &backup_running_href(&id), Some(session())).await;
        assert!(!running_body.contains("style="));
    }

    // ---- t35 통합: 실제 자식 spawn → 중계 → 로그 적재 → 이력 관통 ----
    //
    // 백업은 실제 DB가 필요해 이 테스트에 부적합하다(리더 지시서). `doctor --json`은
    // config 없이도 오프라인으로 돌며 stderr에 진단을 남기므로(설정 오류 exit 2 경로가
    // 오히려 이 테스트에 좋다 — stderr가 확실히 나온다), `spawn_tracked_job`의 통합
    // 테스트 대역으로 쓴다. `spawn_tracked_job`은 명령을 가리지 않으므로(모듈 헤더) 이
    // 테스트가 실제로 검증하는 파이프라인은 `POST /backup`이 쓰는 것과 동일하다.

    /// spawn → stdout/stderr 중계 → `JobLogSink` 적재 → `JobStore` 이력 기록 →
    /// `JobEvent::Done` publish까지 실제 자식으로 한 번 관통시킨다.
    #[tokio::test]
    async fn doctor_job_flows_through_relay_log_and_history() {
        let ctx = Arc::new(ServeConfig::for_test());
        // config_path를 주지 않는다 — doctor가 config 없이 exit 2로 끊기면서 stderr에
        // 사람이 읽는 진단을 남기므로, "stderr가 로그 파일에 실제로 적재되는지"를 굳이
        // 성공 경로를 만들지 않고도 확인할 수 있다.
        let spec = JobSpec::new(JobCommand::Doctor, Lang::En);

        let id = spawn_tracked_job(&ctx, spec, "test")
            .await
            .expect("spawn 실패");
        // **이 잡의** 허브가 등록됐는지 본다. 예전에는 레지스트리 전체 개수가 1 늘었는지를
        // 봤는데, 그때 레지스트리는 프로세스 전역이라 같은 테스트 바이너리의 다른 테스트가
        // 자기 허브를 등록/해제하는 순간 그 개수가 흔들렸다 — 실제로 그 경합으로 이 테스트가
        // 깨졌다. 지금은 레지스트리가 `ctx.live`(이 컨텍스트 전용)라 개수를 봐도 안전하지만,
        // 그래도 이 테스트가 확인하려던 것은 "이 잡의 허브가 생겼다"이므로 그것을 직접 본다.
        assert!(
            ctx.live.hub_for(&id).is_some(),
            "SSE 허브가 등록되지 않았다"
        );

        // 잡이 끝나고 완료 처리(이력 기록 + note 로그)까지 끝나기를 기다린다. 폴링
        // 간격은 이 크레이트의 다른 통합 테스트(`t12` 자식 대기 등)와 같은 자리수다.
        let store = jobs::shared(&ctx.state_dir);
        let mut summary = None;
        for _ in 0..200 {
            let history = store.list().await;
            if let Some(entry) = history.entries.iter().find(|e| e.id == id) {
                if !e_is_running(entry) {
                    summary = Some(entry.clone());
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let summary = summary.expect("잡이 10초 안에 끝나지 않았다");
        assert_eq!(summary.command, "doctor");
        assert!(summary.outcome.is_some(), "종료 기록이 없다");

        // t35의 핵심 — 실제로 파일을 읽어 stderr가 적재됐는지 확인한다.
        let detail = store.detail(&id).await;
        assert!(
            detail.logs.iter().any(|l| l.stream == LogStream::Stderr),
            "stderr가 잡 로그 파일에 적재되지 않았다: {:?}",
            detail.logs
        );
        // 완료 안내(note)도 t13 present()를 거쳐 적재됐다.
        assert!(
            detail.logs.iter().any(|l| l.stream == LogStream::Note),
            "종료 안내 note가 적재되지 않았다: {:?}",
            detail.logs
        );
    }

    fn e_is_running(entry: &crate::web::state::jobs::JobSummary) -> bool {
        entry.is_running()
    }
}
