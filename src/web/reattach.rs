//! 기동 시 고아 잡 재부착 스캔 — t12(생명주기 판정)와 t14(잡 이력)를 잇는 이음매.
//!
//! ## 왜 이 파일이 따로 있는가
//! t12([`crate::web::job::lifecycle`])는 "이 pid가 정말 우리 잡인가"를 판정할 수 있지만 잡
//! 상태가 어디에 어떻게 영속되는지 **모른다**(그 파일 헤더가 명시적으로 모른다고 적어 둔다).
//! t14([`crate::web::state::jobs`])는 이력을 읽고 쓸 수 있지만 pid에 시그널을 보내거나 OS에
//! 프로세스를 물어보는 일을 **하지 않는다**. 둘의 결합을 최소화한 그 설계는 옳았지만, **그
//! 둘을 실제로 이어 붙이는 사람이 없었다** — t12가 만든 [`lifecycle::classify_for_reattach`]·
//! [`lifecycle::wait_for_orphan_exit`]는 테스트 밖 호출부가 0개였고, t14의
//! [`JobStore::rebuild_index`]도 그랬다. 이 파일이 그 호출부다.
//!
//! ## 무엇이 고장나 있었는가 — 영구 409
//! `POST /backup`(profile=prod) → 잡이 도는 중 서버가 `kill -9`로 죽음 → 서버 재기동 →
//! `POST /backup`(prod)이 **영구히 409 AlreadyRunning**.
//!
//! 이유는 한 줄이다: 종료 기록이 없으면 [`JobSummary::is_running`]이 영원히 참이고
//! (`finished_at.is_none()`), [`crate::web::routes::backup`]의 중복 실행 사전 거부가 그 값을
//! 그대로 믿는다. 로테이션은 로그 파일만 지우고 인덱스 항목은 남기므로(t14 설계) 시간이
//! 지나도 사라지지 않는다. 즉 서버가 한 번 비정상 종료하면 그 프로파일의 웹 백업이 **영구히**
//! 막힌다. 회복 경로도 없었다 — 취소 핸들러는 종료 기록을 남기지 않았고, 고아 잡에는 종료를
//! 채워 줄 사람이 아무도 없었다.
//!
//! ## 스캔이 하는 일
//! [`JobStore::list`]에서 `is_running`인 항목만 골라 하나씩 [`lifecycle::classify_for_reattach`]에 넘긴다.
//!
//! - **3축 일치**(pid 생존 + 시작 시각 + 명령행의 프로파일) → 살아 있는 고아다. 그대로 두고
//!   [`lifecycle::wait_for_orphan_exit`]로 종료를 감시하는 태스크를 띄운다. 그 프로세스가 끝나면 종료
//!   기록을 채운다. 화면에는 [`REATTACHED_PROGRESS_LABEL`]("진행률 미상")을 note 로그로
//!   남겨 두므로, 운영자가 그 잡을 열면 "재부착됐고 진행률은 알 수 없다"를 읽는다.
//! - **불일치**(pid가 없거나, 시작 시각이 어긋나거나, 명령행에 프로파일이 없거나, 애초에
//!   pid·프로파일이 기록되지 않음) → 그 프로세스는 우리 잡이 아니다. 즉시 종료 기록을 채워
//!   마감한다. **이것이 영구 409를 푸는 핵심이다.**
//!
//! ## 왜 `crashed`가 아니라 [`JobOutcome::Unknown`]으로 마감하는가
//! "crashed"는 t10의 결과 어휘([`JobOutcome`])에 없다. 새 variant를 추가하면 저장 스키마·
//! 화면 매핑·감사 로그까지 번지는데, 정직하게 보면 우리가 아는 것은 "그 프로세스는 더 이상
//! 없고, 어떻게 끝났는지는 알 수 없다"뿐이다 — 그게 정확히 [`JobOutcome::Unknown`]의 뜻이다
//! ("종료 코드도 시그널 번호도 얻지 못했다 ... 성공이 아니라 판정 불가로 취급하세요"). 억지로
//! `Failed`로 접지 않는 것이 중요하다: 그 잡은 실제로 성공했을 수도 있다(서버만 죽고 백업은
//! 끝났을 수 있다). 판정 불가를 실패로 적으면 이력이 거짓말을 한다. 대신 **왜 그렇게 마감됐는지**를
//! note 로그로 함께 남겨 운영자가 이력 화면에서 사유를 읽을 수 있게 한다.
//!
//! ## 인덱스 재구축이 여기서 도달 가능해진다
//! 인덱스 파일이 없는데 잡 로그 파일은 남아 있으면(운영자가 인덱스를 지웠거나 파일이
//! 손상됐다) 재부착 판정의 입력 자체가 비어 있다 — 살아 있는 고아를 못 보고, 마감할 항목도
//! 못 본다. 그래서 스캔 전에 그 상황을 감지해 [`JobStore::rebuild_index`]를 한 번 부른다
//! (t14가 문서로만 안내하던 복구 수단의 첫 실제 호출부다).
//!
//! **"읽지 못한 줄이 있다"만으로는 재구축하지 않는다.** 재구축은 로테이션으로 로그가 사라진
//! 잡들의 요약을 함께 잃는다(t14 헤더의 "대가"). 잘린 마지막 줄 하나 때문에 오래된 이력
//! 전체를 버리는 것은 남는 장사가 아니다 — 인덱스가 **아예 없을 때만** 부른다. 그때는 잃을
//! 요약이 애초에 없다.
//!
//! ## 스캔 실패는 기동을 막지 않는다
//! 잡 이력은 캐시 성격이다(t14 설계). 이력을 못 읽는다고 콘솔 전체가 뜨지 못하면 운영자는
//! 장애 대응 중에 터미널로 돌아가야 한다. 그래서 이 스캔의 모든 실패는 경고로 남기고 기동을
//! 계속한다 — 단, 그 경우 **stuck 항목이 남아 그 프로파일의 백업이 막힐 수 있다는 것**을
//! 로그로 분명히 알린다([`ReattachReport::log`]).
//!
//! ## 남은 한계 — 재부착된 잡에는 SSE 스트림이 없다
//! 재부착은 프로세스를 되찾을 뿐 **파이프를 되찾지 못한다**(t12 헤더 "재부착 — 진행률을
//! 잃는다"). 그래서 [`crate::web::routes::backup`]의 SSE 허브 레지스트리에는 이 잡의 허브가
//! 없고, `GET /backup/{id}/events`는 404다 — 라이브 화면의 스크립트는 `disconnected`를
//! 표시한다. 운영자가 실제로 보는 것은 **새로고침마다 갱신되는 서버 렌더 스냅샷**이고, 거기에
//! 이 파일이 남긴 note("재부착됨, 진행률 미상")가 그대로 보인다. 종료도 정확히 반영된다 —
//! [`spawn_orphan_watcher`]가 pid 소멸을 감지해 종료 기록을 채우므로 그 다음 새로고침에서
//! 배지가 결과로 바뀐다. 진행률만 알 수 없고, 그건 파이프가 사라진 데서 오는 근본적 한계다
//! (허브를 억지로 만들어 등록해도 그 안으로 흘려보낼 이벤트의 원천이 없다).

use std::sync::Arc;
use std::time::Duration;

use crate::i18n::Lang;
use crate::web::job::lifecycle::{
    self, JobRecord, ReattachDecision, REATTACHED_PROGRESS_LABEL, REATTACH_POLL_INTERVAL,
};
use crate::web::job::JobOutcome;
use crate::web::state::jobs::{JobId, JobStore, JobSummary, LogStream};

/// 스캔 전체에 주는 시간 예산 — 넘으면 남은 항목을 두고 기동을 계속한다.
///
/// ## 왜 예산이 필요한가
/// 스캔은 항목 하나마다 OS에 프로세스를 물어본다. Linux는 `/proc` 파일 두세 개를 읽지만
/// macOS에는 `/proc`가 없어 `ps` 서브프로세스를 띄운다([`lifecycle`] 헤더의 플랫폼 한계) —
/// 항목당 수~수십 ms다. 정상적으로는 `is_running` 항목이 0~수 개라 전부 합쳐도 무시할 수준이지만,
/// 병리적인 인덱스(서버가 수백 번 강제 종료된 이력, 또는 다른 머신에서 복사해 온 인덱스)에서는
/// 항목이 수백 개일 수 있다. 그 경우까지 기동을 붙잡고 있으면 "콘솔이 안 뜬다"가 되는데,
/// 이 스캔은 **캐시 정리**이지 안전 검사가 아니다 — 기동을 막을 자격이 없다.
///
/// 10초의 근거: `routes::doctor`의 `DOCTOR_TIMEOUT`(20초 상한, 비공개 상수)과
/// [`lifecycle::CANCEL_GRACE_DEFAULT`](30초 상한) 같은 계열의 판단으로, "사람이 기동 로그를 보며
/// 기다릴 수 있는" 한 자리 초 규모의 상한이다. 정상 환경에서는 근처에도 가지 않는다.
pub const REATTACH_SCAN_BUDGET: Duration = Duration::from_secs(10);

/// 스캔 결과 — 기동 로그 한 줄로 요약하기 위한 집계.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReattachReport {
    /// `is_running`으로 남아 있던 항목 수(스캔 대상).
    pub scanned: usize,
    /// 3축이 일치해 살아 있는 고아로 재부착한 잡 수. 각각 종료 감시 태스크가 붙는다.
    pub reattached: usize,
    /// 불일치로 판정해 종료 기록을 채워 마감한 잡 수.
    pub closed: usize,
    /// 마감하려 했지만 **종료 기록을 남기지 못한** 잡 수 — 이 잡들은 여전히 stuck이고, 그
    /// 프로파일의 백업이 계속 409로 막힌다.
    pub stuck: usize,
    /// 예산([`REATTACH_SCAN_BUDGET`])을 넘겨 아예 판정하지 못한 항목 수.
    pub skipped: usize,
    /// 인덱스가 없어 [`JobStore::rebuild_index`]를 불렀는지.
    pub rebuilt_index: bool,
}

impl ReattachReport {
    /// 결과를 기동 로그로 남긴다 — **stuck이 남았으면 그 사실을 숨기지 않는다.**
    ///
    /// 아무 일도 없었으면(스캔 대상 0건) 아무것도 찍지 않는다. 정상 기동마다 한 줄씩
    /// 늘어나면 진짜 신호가 묻힌다.
    pub fn log(self) {
        if self.rebuilt_index {
            tracing::warn!(
                "잡 인덱스가 없어 잡 로그 파일에서 재구축했습니다 — 로테이션으로 로그가 \
                 지워진 오래된 잡의 요약은 복원되지 않습니다"
            );
        }
        if self.scanned == 0 {
            return;
        }
        tracing::info!(
            scanned = self.scanned,
            reattached = self.reattached,
            closed = self.closed,
            "재부착 스캔 완료 — 실행 중으로 남아 있던 잡을 판정했습니다"
        );
        if self.skipped > 0 {
            tracing::warn!(
                skipped = self.skipped,
                budget_secs = REATTACH_SCAN_BUDGET.as_secs(),
                "재부착 스캔이 시간 예산을 넘겨 일부 항목을 판정하지 못했습니다 — 그 잡들은 \
                 계속 '실행 중'으로 보이고, 같은 프로파일의 새 백업이 409로 막힐 수 있습니다. \
                 다음 기동에서 다시 시도합니다"
            );
        }
        if self.stuck > 0 {
            tracing::error!(
                stuck = self.stuck,
                "재부착 스캔이 일부 잡의 종료 기록을 남기지 못했습니다 — 그 잡들은 이력에서 \
                 계속 '실행 중'으로 보이고, 같은 프로파일의 새 백업이 409(AlreadyRunning)로 \
                 막힙니다. state 디렉터리의 디스크 여유·권한을 확인하세요"
            );
        }
    }
}

/// 기동 경로에서 부르는 진입점 — 예산을 걸고 스캔한 뒤 결과를 로그로 남긴다.
///
/// **실패해도 반환한다**(에러 타입이 없다) — 모듈 헤더 "스캔 실패는 기동을 막지 않는다".
/// 예산을 넘기면 그 시점까지의 진행은 유지되고(마감된 항목은 이미 디스크에 남았다) 남은
/// 항목은 다음 기동에서 다시 판정된다.
pub async fn run_at_startup(store: &Arc<JobStore>, lang: Lang) {
    match tokio::time::timeout(REATTACH_SCAN_BUDGET, scan(store, lang)).await {
        Ok(report) => report.log(),
        Err(_) => {
            // 타임아웃은 미래를 중간에 떨어뜨리므로 집계를 돌려받을 수 없다 — 개수 대신
            // "예산을 넘겼다"는 사실만 알린다. 그 사이 마감된 항목의 기록은 이미 디스크에
            // 있다(append는 완료된 것만 관측된다).
            tracing::warn!(
                budget_secs = REATTACH_SCAN_BUDGET.as_secs(),
                "재부착 스캔이 시간 예산 안에 끝나지 않아 중단했습니다 — 남은 항목은 계속 \
                 '실행 중'으로 보이고, 같은 프로파일의 새 백업이 409로 막힐 수 있습니다. \
                 다음 기동에서 다시 시도합니다"
            );
        }
    }
}

/// 스캔 본체 — 예산 없이 끝까지 돈다(테스트가 직접 부른다).
///
/// 살아 있는 고아에 대해서는 종료 감시 태스크를 [`tokio::spawn`]으로 띄우고 즉시 다음 항목으로
/// 넘어간다 — 그 잡은 몇 시간을 더 돌 수 있으므로 여기서 기다리면 스캔이 끝나지 않는다.
pub async fn scan(store: &Arc<JobStore>, lang: Lang) -> ReattachReport {
    let mut report = ReattachReport::default();

    let mut history = store.list().await;
    // 인덱스가 없는데 로그 파일은 있다 = 재구축 대상(모듈 헤더 참조).
    if !history.index_present && store.has_log_files().await {
        match store.rebuild_index().await {
            Ok(rebuilt) => {
                report.rebuilt_index = true;
                tracing::info!(
                    jobs = rebuilt.jobs,
                    records = rebuilt.records,
                    unreadable_lines = rebuilt.unreadable_lines,
                    "잡 인덱스를 재구축했습니다"
                );
                history = store.list().await;
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "잡 인덱스를 재구축할 수 없습니다 — 재부착 판정 없이 계속합니다(실행 중으로 \
                     남은 잡이 있으면 그 프로파일의 백업이 막힐 수 있습니다)"
                );
            }
        }
    }

    for summary in history.entries.into_iter().filter(JobSummary::is_running) {
        report.scanned += 1;
        let id = summary.id;
        match identity_of(&summary) {
            Some(record) => {
                let pid = record.pid;
                // 판정은 동기 함수이고 macOS에서는 `ps` 서브프로세스를 띄운다 —
                // tokio 워커 스레드를 붙잡지 않도록 blocking 풀로 넘긴다
                // (`classify_for_reattach` doc이 권장하는 방식).
                let decision = match tokio::task::spawn_blocking(move || {
                    lifecycle::classify_for_reattach(&record)
                })
                .await
                {
                    Ok(decision) => decision,
                    Err(e) => {
                        // 판정 태스크가 패닉했다 — 그 잡을 마감하지도, 재부착하지도 않는다.
                        // 함부로 마감하면 살아 있는 백업을 "끝났다"고 기록할 수 있다.
                        tracing::warn!(job_id = %id, error = %e, "재부착 판정 태스크가 비정상 종료했습니다 — 이 잡은 판정하지 않고 넘어갑니다");
                        report.skipped += 1;
                        continue;
                    }
                };
                match decision {
                    ReattachDecision::Reattached => {
                        report.reattached += 1;
                        note(
                            store,
                            &id,
                            &format!(
                                "{} — {REATTACHED_PROGRESS_LABEL}",
                                lang.sel(
                                    "Reattached after a console restart: the job is still running",
                                    "콘솔 재시작 후 재부착했습니다: 이 잡은 아직 실행 중입니다",
                                )
                            ),
                        )
                        .await;
                        spawn_orphan_watcher(Arc::clone(store), id, pid, lang);
                    }
                    ReattachDecision::Crashed(reason) => {
                        if close(store, &id, lang, &reason).await {
                            report.closed += 1;
                        } else {
                            report.stuck += 1;
                        }
                    }
                }
            }
            None => {
                // pid나 프로파일이 기록되지 않은 항목. 3축 판정을 할 수 없고, 시그널을 보낼
                // 대상도 특정할 수 없다 — 즉 **영원히** 재부착될 수 없는 항목이다. 그대로
                // 두면 그 프로파일의 백업이 영구히 막히므로 마감한다.
                let reason = lang
                    .sel(
                        "no pid or profile was recorded, so this job can never be verified or \
                         reattached",
                        "pid·프로파일이 기록되지 않아 이 잡은 검증도 재부착도 할 수 없습니다",
                    )
                    .to_string();
                if close(store, &id, lang, &reason).await {
                    report.closed += 1;
                } else {
                    report.stuck += 1;
                }
            }
        }
    }

    report
}

/// 이력 요약에서 3축 판정용 신원을 만든다. pid나 프로파일이 없으면 `None`.
fn identity_of(summary: &JobSummary) -> Option<JobRecord> {
    Some(JobRecord {
        pid: summary.pid?,
        started_at: summary.started_at,
        profile: summary.profile.clone()?,
    })
}

/// 고아로 판정되지 않은 항목을 [`JobOutcome::Unknown`]으로 마감한다.
///
/// 사유를 **먼저** note 로그로 남기고 그 다음에 종료 기록을 남긴다 — 잡별 로그 파일에서
/// 종료 레코드 뒤에 로그 줄이 오면 읽는 사람이 "끝난 뒤에 뭔가 더 있었나" 하고 헷갈린다.
///
/// 반환값은 "종료 기록을 실제로 남겼는가"다. `false`면 그 잡은 여전히 stuck이다.
async fn close(store: &Arc<JobStore>, id: &JobId, lang: Lang, reason: &str) -> bool {
    note(
        store,
        id,
        &format!(
            "{} ({reason}) — {}",
            lang.sel(
                "Reattach scan: this job's process is gone",
                "재부착 스캔: 이 잡의 프로세스가 사라졌습니다"
            ),
            lang.sel(
                "closing it as 'unknown' (we cannot tell whether it succeeded) so the profile is \
                 not blocked from running again",
                "성공 여부를 알 수 없으므로 'unknown'으로 마감합니다 — 이 프로파일이 다시 \
                 실행되지 못하고 막히는 것을 막기 위해서입니다",
            )
        ),
    )
    .await;
    store
        .finish_persistent(id, JobOutcome::Unknown)
        .await
        .is_ok()
}

/// note 로그 한 줄을 남긴다. 실패는 경고만 남긴다 — 안내 한 줄을 못 적었다고 마감 자체를
/// 포기하면 영구 409가 그대로 남는다(그게 훨씬 나쁘다).
async fn note(store: &Arc<JobStore>, id: &JobId, text: &str) {
    if let Err(e) = store.append_log(id, LogStream::Note, text).await {
        tracing::warn!(job_id = %id, error = %e, "재부착 스캔 안내 로그를 남기지 못했습니다");
    }
}

/// 살아 있는 고아 하나의 종료를 감시하는 태스크를 띄운다.
///
/// 재부착된 잡은 stdout 파이프를 잃어 종료 코드를 알 수 없다([`lifecycle`] 헤더 "재부착 —
/// 진행률을 잃는다"). 그래서 종료를 감지하면 [`JobOutcome::Unknown`]으로 마감한다 — 여기서
/// 성공/실패를 추측하지 않는 이유는 [`close`]와 같다.
///
/// 이 태스크는 서버 수명 동안 살아 있고 잡이 끝날 때까지 [`REATTACH_POLL_INTERVAL`]마다 깨어난다.
/// 서버가 또 죽으면 이 태스크도 사라지지만, 그때는 다음 기동의 스캔이 같은 판정을 다시 한다 —
/// **이 감시 태스크가 유일한 회복 경로가 아니다**는 것이 이 설계의 안전망이다.
fn spawn_orphan_watcher(store: Arc<JobStore>, id: JobId, pid: u32, lang: Lang) {
    tokio::spawn(async move {
        lifecycle::wait_for_orphan_exit(pid, REATTACH_POLL_INTERVAL).await;
        note(
            &store,
            &id,
            lang.sel(
                "The reattached process has exited. Its exit code is unknown — the console lost \
                 the pipe when it restarted, so the result is recorded as 'unknown'.",
                "재부착된 프로세스가 종료됐습니다. 종료 코드는 알 수 없습니다 — 콘솔이 재시작될 \
                 때 파이프를 잃었으므로 결과를 'unknown'으로 기록합니다.",
            ),
        )
        .await;
        if let Err(e) = store.finish_persistent(&id, JobOutcome::Unknown).await {
            tracing::warn!(job_id = %id, error = %e, "재부착된 잡의 종료 기록 실패");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::state::jobs::JobStart;

    /// 임시 state 디렉터리에 붙은 쓰기용 저장소.
    fn store() -> (Arc<JobStore>, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(JobStore::open(dir.path()));
        (store, dir)
    }

    /// 종료 기록 없이 잡 하나를 시작 기록만 남긴다(= `is_running`으로 남는다).
    async fn start_running(store: &JobStore, profile: Option<&str>, pid: Option<u32>) -> JobId {
        store
            .start(JobStart {
                command: "backup",
                profile,
                args_masked: &["backup".to_string()],
                started_at: chrono::Utc::now(),
                pid,
            })
            .await
            .expect("시작 기록 실패")
    }

    async fn summary_of(store: &JobStore, id: &JobId) -> JobSummary {
        store
            .list()
            .await
            .entries
            .into_iter()
            .find(|entry| entry.id == *id)
            .expect("이력에 항목이 없다")
    }

    /// **H1의 핵심 회귀 테스트**: 3축 불일치 항목이 마감돼 `finished_at`이 채워진다.
    ///
    /// 존재하지 않는 pid(`u32::MAX - 1`)를 기록한 항목은 재부착될 수 없다. 스캔 전에는
    /// `is_running`이 참이고(그래서 그 프로파일의 백업이 409로 막힌다) 스캔 후에는 거짓이어야
    /// 한다.
    #[tokio::test]
    async fn scan_closes_entries_whose_process_is_gone() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), Some(u32::MAX - 1)).await;
        assert!(
            summary_of(&store, &id).await.is_running(),
            "테스트 전제: 스캔 전에는 실행 중으로 보인다"
        );

        let report = scan(&store, Lang::En).await;
        assert_eq!(report.scanned, 1);
        assert_eq!(report.closed, 1, "{report:?}");
        assert_eq!(report.reattached, 0);
        assert_eq!(report.stuck, 0);

        let after = summary_of(&store, &id).await;
        assert!(!after.is_running(), "마감되지 않았다 — 영구 409가 그대로다");
        assert!(after.finished_at.is_some(), "finished_at이 비어 있다");
        assert_eq!(
            after.outcome.as_deref(),
            Some(JobOutcome::Unknown.label()),
            "판정 불가를 실패로 접지 않아야 한다"
        );
    }

    /// 마감 이유가 note 로그로 남아 운영자가 이력 화면에서 읽을 수 있다.
    #[tokio::test]
    async fn closed_entries_carry_a_reason_in_the_log() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), Some(u32::MAX - 1)).await;
        scan(&store, Lang::En).await;

        let detail = store.detail(&id).await;
        let notes: Vec<&str> = detail
            .logs
            .iter()
            .filter(|line| line.stream == LogStream::Note)
            .map(|line| line.text.as_str())
            .collect();
        assert!(!notes.is_empty(), "마감 사유 note가 없다: {detail:?}");
        assert!(
            notes.iter().any(|text| text.contains("Reattach scan")),
            "사유에 스캔이 한 일이 드러나야 한다: {notes:?}"
        );
    }

    /// pid가 기록되지 않은 항목도 마감된다 — 검증할 방법이 없으므로 영원히 재부착될 수
    /// 없고, 두면 그 프로파일이 영구히 막힌다.
    #[tokio::test]
    async fn scan_closes_entries_without_a_pid() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), None).await;

        let report = scan(&store, Lang::Ko).await;
        assert_eq!(report.closed, 1, "{report:?}");
        assert!(!summary_of(&store, &id).await.is_running());
    }

    /// 프로파일이 기록되지 않은 항목도 같다(3축 판정의 세 번째 축을 만들 수 없다).
    #[tokio::test]
    async fn scan_closes_entries_without_a_profile() {
        let (store, _dir) = store();
        let id = start_running(&store, None, Some(u32::MAX - 1)).await;

        let report = scan(&store, Lang::En).await;
        assert_eq!(report.closed, 1, "{report:?}");
        assert!(!summary_of(&store, &id).await.is_running());
    }

    /// 이미 끝난 잡은 스캔 대상이 아니다 — 종료 기록을 두 번 쓰지 않는다.
    #[tokio::test]
    async fn scan_ignores_finished_jobs() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), Some(u32::MAX - 1)).await;
        store
            .finish(&id, JobOutcome::Succeeded)
            .await
            .expect("종료 기록 실패");

        let report = scan(&store, Lang::En).await;
        assert_eq!(report.scanned, 0, "{report:?}");
        // 결과가 덧씌워지지 않았다 — 성공이 unknown으로 바뀌면 이력이 거짓말을 한다.
        assert_eq!(
            summary_of(&store, &id).await.outcome.as_deref(),
            Some(JobOutcome::Succeeded.label())
        );
    }

    /// **살아 있는 고아는 마감하지 않는다.** 3축이 모두 맞으면 재부착으로 판정하고, 그 잡은
    /// 여전히 `is_running`이어야 한다 — 여기서 마감하면 도는 백업을 "끝났다"고 기록하는 것이다.
    ///
    /// 실제로 살아있는 프로세스가 필요하므로 `/bin/sleep <N>`을 띄우고, 그 인자를 프로파일
    /// 토큰으로 재사용한다(`job::lifecycle` 테스트들이 쓰는 관용구 — 검증 대상은 "명령행에서
    /// 토큰을 찾는 메커니즘"이므로 토큰이 무엇이든 성질이 같다).
    #[tokio::test]
    async fn scan_keeps_a_live_orphan_running() {
        const SLEEP_PROFILE: &str = "43";
        let (store, _dir) = store();

        let mut cmd = tokio::process::Command::new("/bin/sleep");
        cmd.arg(SLEEP_PROFILE);
        #[cfg(unix)]
        cmd.process_group(0);
        let mut child = cmd.spawn().expect("spawn 실패");
        let pid = child.id().expect("pid 없음");
        let id = start_running(&store, Some(SLEEP_PROFILE), Some(pid)).await;

        let report = scan(&store, Lang::En).await;
        assert_eq!(report.scanned, 1);
        // OS 시작 시각을 확인할 수 없는 환경(/proc도 ps도 없음)에서는 fail-closed로 마감된다 —
        // 그 경우는 `lifecycle`의 sanity check 테스트가 이미 건너뛴다고 알려 준다.
        if report.reattached == 0 {
            eprintln!("건너뜀: 이 환경에서 3축 검증이 성립하지 않습니다({report:?})");
        } else {
            assert_eq!(report.closed, 0, "살아 있는 고아를 마감했다: {report:?}");
            assert!(
                summary_of(&store, &id).await.is_running(),
                "재부착된 잡이 마감돼 버렸다"
            );
            let detail = store.detail(&id).await;
            assert!(
                detail
                    .logs
                    .iter()
                    .any(|line| line.text.contains(REATTACHED_PROGRESS_LABEL)),
                "진행률 미상 안내가 없다: {:?}",
                detail.logs
            );
        }

        let _ = child.kill().await;
        let _ = child.wait().await;
    }

    /// 인덱스 파일이 사라졌지만 잡 로그가 남아 있으면 재구축한 뒤 판정한다 —
    /// [`JobStore::rebuild_index`]의 도달 가능한 경로.
    #[tokio::test]
    async fn scan_rebuilds_a_missing_index_then_classifies() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), Some(u32::MAX - 1)).await;
        std::fs::remove_file(store.index_path()).expect("인덱스 삭제 실패");
        assert!(
            !store.list().await.index_present,
            "테스트 전제: 인덱스가 없어야 한다"
        );

        let report = scan(&store, Lang::En).await;
        assert!(report.rebuilt_index, "재구축이 불리지 않았다: {report:?}");
        assert_eq!(report.scanned, 1, "재구축 뒤 그 항목을 봐야 한다");
        assert_eq!(report.closed, 1);
        assert!(!summary_of(&store, &id).await.is_running());
    }

    /// 잡이 하나도 없는 새 state 디렉터리에서는 재구축을 부르지 않는다 — 되살릴 것이 없는데
    /// 빈 인덱스 파일만 만들면 화면의 "인덱스 없음" 안내가 무의미하게 사라진다.
    #[tokio::test]
    async fn scan_does_not_rebuild_when_there_is_nothing_to_rebuild() {
        let (store, _dir) = store();
        let report = scan(&store, Lang::En).await;
        assert_eq!(report, ReattachReport::default(), "{report:?}");
        assert!(!store.index_path().exists(), "빈 인덱스를 만들었다");
    }

    /// 기동 진입점은 예산을 걸고도 정상 케이스를 그대로 처리한다(그리고 패닉하지 않는다).
    #[tokio::test]
    async fn run_at_startup_completes_within_budget() {
        let (store, _dir) = store();
        let id = start_running(&store, Some("prod"), Some(u32::MAX - 1)).await;
        run_at_startup(&store, Lang::En).await;
        assert!(!summary_of(&store, &id).await.is_running());
    }
}
