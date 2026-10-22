//! 내장 스케줄러 — 외부 cron을 대체하는 주기 백업 실행기.
//!
//! ## 이 모듈이 존재하는 이유
//! 지금까지 x-backup의 주기 백업은 **외부 cron**이 했다. `x-backup serve`를 "상주 운영
//! 콘솔"로 만든 선택의 핵심 이득이 바로 이 모듈이다 — cron 항목을 손으로 편집하지 않고
//! 화면에서 스케줄을 등록·수정·삭제하고, 각 실행이 잡 이력·감사 로그에 그대로 남는다.
//! 외부 cron으로는 "이 백업이 어제 왜 실패했나"를 콘솔에서 볼 수 없다.
//!
//! ## 스케줄은 캐시가 아니라 **진짜 상태**다 — 이 모듈 전체를 지배하는 사실
//! [`crate::web::state`] 헤더는 "여기 있는 모든 것은 manifest에서 재구성 가능한 캐시다"라는
//! 규칙을 세웠고, 잡 이력은 실제로 그렇다. **스케줄은 그 규칙의 유일한 예외다.**
//! destination의 manifest에도, 어떤 CLI 출력에도, crontab에도 사본이 없다. 그래서 스케줄
//! 정의가 날아가면 **백업이 조용히 멈춘다** — 운영자는 여전히 백업이 돌고 있다고 믿고, 그
//! 믿음이 깨지는 순간은 복구가 필요해진 순간이다.
//!
//! 그 사실에서 나온 결정들은 [`crate::web::state::schedules`] 헤더가 길게 적어 두었다(요약:
//! append-only NDJSON이 아니라 단일 JSON + 원자적 교체 · 직전본 한 세대 · 손상 시
//! fail-closed가 아니라 **fail-loud**).
//!
//! ## 시각 기준은 UTC다 — DST를 다루는 방식
//! cron 표현식의 모든 필드는 **UTC로 해석된다.** 로컬 시각으로 해석하면 DST 전환에서 두
//! 사고가 구조적으로 생긴다: 봄 전환에서 존재하지 않는 시각(02:30)을 가리켜 **그날 백업이
//! 누락되고**, 가을 전환에서 같은 시각이 두 번 와 **같은 백업이 두 번 떠** 뒤쪽이 파일 락에
//! 걸린다(exit 5 — 이력에는 실패처럼 쌓인다). UTC에는 DST가 없어 두 사고가 표현 자체로
//! 불가능하다. 자세한 근거와 대가는 [`cron`] 모듈 헤더 "계산 기준은 UTC다" 참조.
//!
//! **대가는 화면이 갚는다.** 운영자가 "매일 03:00(로컬)"을 기대하면 어긋나므로,
//! [`crate::web::view::schedule`]은 (1) 표현식 입력 라벨을 `Cron (UTC)`로 찍고, (2) 다음 실행
//! 시각을 **UTC 표기와 서버 로컬 표기 두 줄로** 보여주고, (3) 화면 상단에 기준이 UTC라는
//! 안내를 고정으로 둔다. 어느 쪽이 기준인지 운영자가 코드가 아니라 화면에서 읽어야 한다.
//!
//! ## 놓친 실행 — **건너뛴다.** 대신 놓쳤다는 사실을 크게 알린다
//! 서버가 꺼져 있는 동안 지나간 실행 시각은 재기동 때 **실행하지 않는다.** 근거:
//!
//! 1. **즉시 실행(catch-up)은 실제로 위험하다.** 매시 스케줄이 있는 서버가 하루 꺼져 있으면
//!    24개의 백업이 기동 직후 한꺼번에 몰린다. 전부 같은 프로파일이라 하나만 락을 잡고
//!    나머지 23개는 exit 5로 끊기며, 이력에는 "실패" 23줄이 남는다. 운영자가 아침에 보는
//!    화면이 그것이면 진짜 문제를 찾지 못한다.
//! 2. **지나간 시각의 백업은 지금 실행해도 그 시각의 데이터가 아니다.** 어제 03:00 백업을
//!    오늘 09:00에 돌려서 얻는 것은 "오늘 09:00 백업"이다. 그건 운영자가 버튼을 누르면 되는
//!    일이고, 스케줄러가 몰래 결정할 일이 아니다.
//! 3. **운영자가 실제로 필요한 것은 백업이 아니라 그 사실(gap)이다.** 콘솔이 꺼져 있던 동안
//!    백업이 빠졌다는 것은 RPO 정보다. 그래서 기동 시 스케줄별로 **몇 번을 놓쳤는지** 세어
//!    로그와 화면에 남긴다([`crate::web::state::schedules::Schedule::missed_since`]).
//!
//! 이 정책은 코드 구조로 강제된다 — 루프의 커서가 **서버 기동 시각에서 시작하므로**
//! 기동 전의 어떤 시각도 후보가 될 수 없다([`ScheduleClock`]).
//!
//! ### 다만 "우리가 늦은 것"은 다르다 — [`LATE_FIRE_LIMIT`]
//! 서버가 켜져 있는데도 틱이 늦는 경우가 있다(노트북 절전, 극심한 부하). 그때 지나간 시각은
//! "서버가 꺼져 있던" 것과 성질이 다르다 — 스케줄은 살아 있었고 우리가 늦었을 뿐이다. 그래서
//! **루프의 정체 시간**이 [`LATE_FIRE_LIMIT`] 안이면 **한 번만** 발화한다(밀린 개수만큼이
//! 아니라 가장 최근 것 하나로 합친다). 그 한도를 넘었으면 발화하지 않고 크게 로그만 남긴다 —
//! 사흘 절전된 노트북이 깨어나면서 예고 없이 프로덕션 백업을 시작하는 것은 놀람이지
//! 서비스가 아니다.
//!
//! 지연을 "스케줄이 얼마나 늦었나"가 아니라 "루프가 얼마나 잠들어 있었나"로 재는 이유는
//! [`ScheduleClock::begin_tick`] doc에 있다 — 전자로는 사흘 절전을 볼 수 없다.
//!
//! ## 중복 실행 방어 — 세 겹
//! 1. **사전 확인.** 발화 직전에 잡 이력에서 같은 프로파일의 실행 중 잡을 찾는다. 있으면 이
//!    발화를 건너뛴다(로그만 남기고 실패로 세지 않는다). 우리 잡은 모두 `backup --profile X`
//!    이므로 "같은 프로파일이 도는 중"은 "같은 스케줄이 도는 중"을 포함한다 — 별도의
//!    스케줄별 in-flight 상태를 둘 필요가 없다.
//! 2. **커서 전진.** 건너뛴 발화도 커서를 전진시킨다. 그러지 않으면 같은 시각이 매 틱마다
//!    다시 후보가 되어 로그를 30초마다 채운다.
//! 3. **exit 5 표면화.** 락은 자식이 잡으므로(웹은 락을 재구현하지 않는다) 1번과 실제 락
//!    획득 사이에는 경합 창이 있다. 그 창에 걸린 자식은 exit 5로 끊기고, t10/t13/t14 전
//!    구간이 그것을 **실패가 아니라 경고**로 접는다([`crate::web::routes::backup`] 헤더의
//!    같은 설계).
//!
//! ## 감사 로그 — `scheduler`와 `web`을 구분한다
//! 스케줄러가 띄운 잡은 [`crate::web::routes::backup::spawn_tracked_job`]에
//! `actor = `[`SCHEDULER_ACTOR`]`("scheduler")`로 들어간다. 사람이 버튼을 누른 잡은
//! `"web"`이다. 이 구분이 없으면 감사 로그를 읽는 사람이 "새벽 3시에 누가 백업을 돌렸나"를
//! 판단할 수 없다 — 자동 실행과 사람의 행동은 감사에서 완전히 다른 사건이다.
//!
//! 스케줄 CRUD 자체는 더 강한 강제를 받는다: [`crate::web::state::schedules::ScheduleStore`]의
//! 쓰기 메서드가 [`crate::web::audit::AuditReceipt`]를 **값으로** 요구하므로, 감사 기록 없이
//! 스케줄을 바꾸는 코드는 컴파일되지 않는다(t27의 `ConfigStore::apply`와 같은 형태).
//!
//! ## 외부 crontab과의 이중 실행(t30)
//! 기존 cron 사용자가 콘솔에도 같은 스케줄을 등록하면 두 프로세스가 같은 시각에 백업을
//! 띄운다. 파일 락이 exit 5로 막아 주므로 **데이터는 안전하지만**, 운영자에게는 "백업 실패"로
//! 보인다. 그래서 기동 시와 스케줄 등록 화면에서 `crontab -l`을 읽어 `x-backup` 항목이 있으면
//! 경고한다([`detect_crontab`]). 셸을 경유하지 않고, 실패는 기동을 막지 않는다 — crontab이
//! 없거나 권한이 없는 것은 완전히 정상이다.
//!
//! ## 종료
//! 루프는 [`tokio::spawn`]으로 떠 있고 별도의 종료 신호를 받지 않는다. `axum`의 graceful
//! shutdown이 끝나면 프로세스가 종료되며 루프도 함께 사라진다. 스케줄러가 띄운 자식은
//! 웹에서 띄운 잡과 완전히 같은 처지가 된다 — 계속 돌고, 다음 기동에서
//! [`crate::web::reattach`]가 재부착한다. 그래서 이 모듈에 종료 처리를 따로 두지 않는다.

pub mod cron;

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::web::job::args::ProfileName;
use crate::web::job::{JobCommand, JobSpec};
use crate::web::mask::SecretRegistry;
use crate::web::state::jobs::JobStore;
use crate::web::state::schedules::{self, Schedule, ScheduleId, ScheduleStore};
use crate::web::ServeConfig;

/// 스케줄러가 띄운 잡의 감사 로그 `actor`.
///
/// 사람이 누른 잡은 `"web"`이다([`crate::web::routes::backup`]의 `AUDIT_ACTOR`). 이 구분이
/// 감사 로그의 핵심 정보다(모듈 헤더 "감사 로그" 참조).
pub const SCHEDULER_ACTOR: &str = "scheduler";

/// 한 번의 대기 상한.
///
/// 30초의 근거: 이 값이 곧 **새로 등록한 스케줄이 루프에 보이기까지의 최대 지연**이다.
/// 운영자가 화면에서 스케줄을 만든 직후 "정말 등록됐나"를 확인하려면 그 지연이 사람 척도
/// (수십 초) 안이어야 한다. 동시에 이 값은 순수한 깨어남 비용이므로(할 일이 없으면 즉시 다시
/// 잔다) 짧게 두는 대가가 거의 없다.
///
/// 이 상한이 필요한 더 근본적인 이유는 [`tokio::time::sleep`]이 **모노토닉 시계**를 쓴다는
/// 것이다 — 시스템 절전 중에는 그 시계가 멈추는 플랫폼이 있어, "다음 실행까지 6시간" 같은
/// 긴 잠에 들면 절전에서 깨어난 뒤 6시간을 더 기다릴 수 있다. 잠을 짧게 끊고 매번 벽시계
/// ([`Utc::now`])를 다시 보면 그 오차가 최대 이 값으로 묶인다.
pub const SCHEDULER_TICK_MAX: Duration = Duration::from_secs(30);

/// 한 번의 대기 하한 — 경계 시각에서 바쁜 루프가 되는 것을 막는다.
const SCHEDULER_TICK_MIN: Duration = Duration::from_millis(250);

/// "우리가 늦은 것"으로 보고 발화를 허용하는 **루프 정체 시간**의 한도(모듈 헤더 "다만
/// 우리가 늦은 것은 다르다").
///
/// 비교 대상이 무엇인지가 중요하다 — 스케줄이 얼마나 늦었는지가 아니라 **직전 틱과 이번 틱
/// 사이의 벽시계 간격**이다. 그 이유는 [`ScheduleClock::begin_tick`] doc에 있다(요약: 매시
/// 스케줄은 사흘 절전 후에도 "30분밖에 늦지 않았다"로 보이고, 일간 스케줄의 커서는 정상
/// 운영 중에도 24시간 낡아 있다 — 둘 다 지연의 척도가 될 수 없다).
///
/// 1시간의 근거: 정상 운영의 틱 간격은 [`SCHEDULER_TICK_MAX`](30초) 이하다. 1시간을 넘긴
/// 정체는 서버가 사실상 멈춰 있었다는 뜻이고, 그 상태에서 깨어나며 시작하는 백업은 "예고
/// 없는 프로덕션 백업"이 된다. 한도를 분 단위로 짧게 두면 정상적인 노트북 뚜껑 닫기에서도
/// 스케줄이 조용히 빠지므로, 사람이 "잠깐 자리를 비웠다"고 느끼는 규모의 상한으로 잡았다.
pub const LATE_FIRE_LIMIT: chrono::TimeDelta = chrono::TimeDelta::hours(1);

/// crontab 조회 명령. **셸을 경유하지 않는다** — 인자를 직접 넘긴다.
const CRONTAB_BIN: &str = "crontab";

/// crontab 조회 타임아웃. 이 조회는 진단이므로 기동을 붙잡을 자격이 없다.
const CRONTAB_PROBE_TIMEOUT: Duration = Duration::from_secs(3);

/// crontab 항목에서 우리를 식별하는 토큰.
///
/// 줄 전체에 대한 `contains`가 아니라 **토큰의 마지막 경로 조각**과 비교한다
/// ([`mentions_x_backup`]) — 그 이유는 그 함수의 문서에 있다.
const CRONTAB_MARKER: &str = "x-backup";

/// crontab 항목 한 줄을 보여줄 때의 길이 상한 — 로그와 마크업을 묶는다.
const CRONTAB_LINE_MAX: usize = 200;

/// crontab 항목을 최대 몇 개까지 모을지. 화면에 수십 줄을 쏟지 않는다.
const CRONTAB_ENTRIES_MAX: usize = 20;

// ---------------------------------------------------------------------------
// crontab 충돌 감지(t30)
// ---------------------------------------------------------------------------

/// `crontab -l` 조회 결과.
///
/// **항목 텍스트는 이미 마스킹되어 있다** — [`detect_crontab`]이 [`SecretRegistry`]를
/// 통과시킨 뒤에만 이 타입을 만든다. 타입 안에 원문이 들어올 경로를 두지 않는 것이
/// 요점이다: 이 값은 기동 로그와 화면 두 곳으로 가고, 두 곳이 각자 마스킹하기로 하면
/// 한쪽이 조용히 빠질 수 있다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CrontabScan {
    /// `x-backup`을 언급하는 crontab 항목(마스킹·길이 제한 적용).
    pub entries: Vec<String>,
    /// 조회하지 못한 사유(있으면). **이것은 오류가 아니다** — crontab이 설치되지 않았거나
    /// 이 사용자에게 crontab이 없는 것은 정상이다.
    pub unavailable: Option<String>,
}

impl CrontabScan {
    /// 이중 실행 위험이 있는지.
    pub fn conflicts(&self) -> bool {
        !self.entries.is_empty()
    }

    /// 기동 로그로 남긴다. 충돌이 없으면 아무것도 찍지 않는다(조회 실패도 조용하다 —
    /// crontab이 없는 것은 정상이므로 매 기동 경고를 내면 진짜 신호가 묻힌다).
    pub fn log(&self) {
        if !self.conflicts() {
            if let Some(reason) = &self.unavailable {
                // debug 수준 — 진단할 때만 필요하다.
                tracing::debug!(reason = %reason, "crontab을 조회하지 않았습니다");
            }
            return;
        }
        tracing::warn!(
            entries = self.entries.len(),
            "crontab에 x-backup 항목이 있습니다 — 콘솔 스케줄러와 이중 실행될 수 있습니다. \
             파일 락이 데이터를 보호하므로 손상되지는 않지만, 뒤에 시작한 쪽이 exit 5로 끊겨 \
             이력에 경고가 쌓입니다. 콘솔 스케줄로 옮겼다면 crontab 항목을 제거하세요"
        );
        for entry in &self.entries {
            tracing::warn!(entry = %entry, "crontab 항목");
        }
    }
}

/// `crontab -l`을 읽어 `x-backup` 항목을 찾는다.
///
/// ## 실패가 정상인 조회다
/// crontab 바이너리가 없거나(컨테이너 이미지에 흔하다), 이 사용자에게 crontab이 없거나
/// (`crontab: no crontab for op` — exit 1), 권한이 없을 수 있다. 그 전부가 정상이고,
/// [`CrontabScan::unavailable`]에 사유만 담아 조용히 돌아온다. **어떤 경우에도 기동을 막지
/// 않는다.**
///
/// ## 셸을 경유하지 않는다
/// [`std::process::Command::new`]에 인자를 직접 준다. `sh -c "crontab -l"`을 쓰면 셸
/// 메타문자 해석·PATH 조작·인용 실수가 전부 표면이 되는데, 얻는 것은 없다(파이프도
/// 리다이렉션도 쓰지 않는다).
///
/// ## 파싱을 최소로 한다
/// cron 문법을 여기서 해석하지 않는다. 주석(`#`)이 아닌 줄에 `x-backup`이 들어 있으면
/// 항목으로 본다. 정확한 필드 파싱을 시도하면 시스템 crontab(사용자 필드가 하나 더 있다)·
/// `MAILTO=` 같은 env 줄·`@reboot` 별칭까지 다뤄야 하는데, **우리가 답해야 할 질문은
/// "충돌 가능성이 있는가" 하나**다. 거짓 양성(주석 아닌 줄에 우연히 x-backup 언급)은 경고
/// 한 줄이고, 거짓 음성은 이중 실행이다 — 넓게 잡는 쪽이 맞다.
pub async fn detect_crontab(registry: &SecretRegistry) -> CrontabScan {
    let mut command = tokio::process::Command::new(CRONTAB_BIN);
    command
        .arg("-l")
        // 자식이 stdin을 기다리며 멈추는 경로를 없앤다(잡 러너와 같은 원칙).
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());

    let output = match tokio::time::timeout(CRONTAB_PROBE_TIMEOUT, command.output()).await {
        Ok(Ok(output)) => output,
        Ok(Err(e)) => {
            return CrontabScan {
                entries: Vec::new(),
                unavailable: Some(format!("crontab을 실행할 수 없습니다: {e}")),
            }
        }
        Err(_) => {
            return CrontabScan {
                entries: Vec::new(),
                unavailable: Some(format!(
                    "crontab 조회가 {}초 안에 끝나지 않았습니다",
                    CRONTAB_PROBE_TIMEOUT.as_secs()
                )),
            }
        }
    };

    if !output.status.success() {
        return CrontabScan {
            entries: Vec::new(),
            // 종료 코드만 남긴다 — stderr는 버렸다(경로·사용자명이 섞여 나올 수 있고,
            // 우리에게 필요한 정보가 아니다).
            unavailable: Some(format!(
                "crontab -l이 실패했습니다(exit {:?}) — 이 사용자에게 crontab이 없을 수 있습니다",
                output.status.code()
            )),
        };
    }

    let text = String::from_utf8_lossy(&output.stdout);
    CrontabScan {
        entries: collect_crontab_entries(&text, registry),
        unavailable: None,
    }
}

/// crontab 본문에서 우리 항목을 골라 마스킹·절단한다.
///
/// 순수 함수로 떼어 둔 이유: 실제 `crontab`이 없는 환경(CI 컨테이너)에서도 파싱 규칙을
/// 단위 테스트로 고정할 수 있다.
fn collect_crontab_entries(text: &str, registry: &SecretRegistry) -> Vec<String> {
    let mut entries = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        if !mentions_x_backup(trimmed) {
            continue;
        }
        // crontab 항목에 자격증명이 인라인으로 들어간 경우(예: `XB_URI=mongodb://u:p@h`)가
        // 실제로 존재한다. 화면·로그로 나가기 전에 지운다.
        //
        // **두 겹인 이유**: `registry`는 config가 아는 값만 안다. crontab 줄의 자격증명은
        // 콘솔보다 먼저 있었거나 다른 계정을 쓰는 경우가 많아 config에 없을 수 있고, 그러면
        // `mask`가 통째로 지나쳐 원문이 화면에 실린다. 그래서 값의 내용을 모르는 채로도
        // 지울 수 있는 `NAME=VALUE` 구문부터 먼저 무너뜨린다.
        let redacted = redact_inline_env(trimmed);
        let masked = registry.mask(&redacted);
        let shown = if masked.chars().count() > CRONTAB_LINE_MAX {
            let head: String = masked.chars().take(CRONTAB_LINE_MAX).collect();
            format!("{head}…")
        } else {
            masked
        };
        entries.push(shown);
        if entries.len() >= CRONTAB_ENTRIES_MAX {
            break;
        }
    }
    entries
}

/// 명령이 x-backup **실행**을 담고 있는가.
///
/// 토큰의 마지막 경로 조각이 정확히 [`CRONTAB_MARKER`]일 때만 참이다. 줄 전체를
/// `contains`로 보면 `>> /var/log/x-backup.log` 같은 리다이렉트가 전부 걸려, 아무 문제 없는
/// crontab에도 매번 경고가 뜬다. 그렇게 되면 **진짜 충돌이 왔을 때 아무도 그 경고를 읽지
/// 않는다** — 이 경고의 가치는 드물게 뜨는 데 있다.
fn mentions_x_backup(line: &str) -> bool {
    line.split_whitespace().any(|token| {
        // 셸 구두점을 떼어낸다: `cd /srv && ./x-backup backup;`에서 토큰이 `x-backup;`이 된다.
        let token = token.trim_matches(|c: char| "\"'`();&|<>".contains(c));
        token.rsplit('/').next() == Some(CRONTAB_MARKER)
    })
}

/// 줄 안의 `NAME=VALUE` 환경 대입에서 **값만** 지운다.
///
/// 이름은 남긴다 — `XB_PROD_URI=<redacted>`는 운영자가 crontab에서 그 줄을 찾는 열쇠이고,
/// env 이름은 config에도 평문으로 있어 무해하다(`server::warn_unmaskable_secrets`와 같은
/// 판단). 값이 있어야 뜻이 통하는 플래그(`--profile=prod`)는 건드리지 않는다 — 자격증명이
/// 사는 자리는 플래그가 아니라 셸 환경 대입이다.
fn redact_inline_env(line: &str) -> String {
    line.split_whitespace()
        .map(|token| match token.split_once('=') {
            Some((name, _)) if is_env_name(name) => format!("{name}=<redacted>"),
            _ => token.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// 환경변수 이름으로 쓸 수 있는 문자열인가(대입 구문 판정용).
fn is_env_name(name: &str) -> bool {
    !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

// ---------------------------------------------------------------------------
// 커서 — "어디까지 평가했는가"
// ---------------------------------------------------------------------------

/// 스케줄별 평가 커서.
///
/// ## 이 타입이 놓친 실행 정책을 **구조로** 강제한다
/// 커서는 "이 시각까지는 이미 판단했다"는 표시다. 새 스케줄을 처음 볼 때 커서를 **그 순간의
/// 시각**으로 놓기 때문에, 서버 기동 전이나 스케줄 등록 전의 어떤 시각도 발화 후보가 될 수
/// 없다 — 놓친 실행을 건너뛴다는 정책이 조건문이 아니라 초기값에서 나온다(모듈 헤더).
///
/// 디스크에 저장하지 않는다. 저장하면 그것이 곧 "재기동 시 catch-up"의 재료가 되고, 우리는
/// 그것을 하지 않기로 했다. 놓친 실행의 **보고**는 디스크의 `last_run_at`으로 따로 계산한다.
#[derive(Debug, Default)]
struct ScheduleClock {
    cursors: HashMap<ScheduleId, DateTime<Utc>>,
    /// 직전 틱의 벽시계 시각 — "우리가 얼마나 오래 잠들어 있었나"의 유일한 측정값.
    last_tick: Option<DateTime<Utc>>,
}

/// 한 스케줄에 대한 이 틱의 판정.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Due {
    /// 발화할 시각(하나로 합쳐진 값)과 함께 건너뛴 개수.
    Fire { at: DateTime<Utc>, coalesced: usize },
    /// 지나갔지만 [`LATE_FIRE_LIMIT`]를 넘겨 발화하지 않는다.
    TooLate { at: DateTime<Utc>, coalesced: usize },
    /// 아직 시간이 아니다.
    NotYet,
}

impl ScheduleClock {
    /// 틱을 시작하고 **루프가 잠들어 있던 시간**을 돌려준다.
    ///
    /// ## 왜 지연을 이렇게 측정하는가
    /// 처음에는 "가장 최근 후보가 얼마나 늦었나"로 판정했는데 그것은 틀렸다. 매시 스케줄이
    /// 있는 서버가 사흘 절전됐다 깨어나면 가장 최근 후보는 **30분밖에 늦지 않았다**(직전
    /// 정시). 그 값으로는 "사흘 죽어 있었다"를 볼 수 없다.
    ///
    /// 커서 나이(`now - cursor`)도 틀렸다. 커서는 발화할 때만 전진하므로, 매일 도는
    /// 스케줄은 **정상 운영 중에도** 커서가 24시간 낡아 있다 — 그러면 모든 일간 스케줄이
    /// 영구히 "너무 늦었다"로 판정된다.
    ///
    /// 옳은 측정값은 스케줄의 성질이 아니라 **루프 자신의 성질**이다: 직전 틱과 이번 틱
    /// 사이의 벽시계 간격. 정상 운영에서는 [`SCHEDULER_TICK_MAX`] 이하이고, 절전·정지에서는
    /// 그 시간만큼 벌어진다. 스케줄 주기와 무관하게 항상 옳다.
    ///
    /// 첫 틱은 지연 0이다(비교할 직전 틱이 없다). 그 시점에는 애초에 발화할 것이 없다 —
    /// 커서가 방금 `now`로 초기화되므로.
    fn begin_tick(&mut self, now: DateTime<Utc>) -> chrono::TimeDelta {
        let stall = match self.last_tick {
            Some(previous) => now - previous,
            None => chrono::TimeDelta::zero(),
        };
        self.last_tick = Some(now);
        stall
    }

    /// 이 스케줄이 지금 발화해야 하는지 판정하고 커서를 전진시킨다.
    ///
    /// 지나간 시각이 여러 개면 **가장 최근 하나로 합친다**(coalesce). 밀린 만큼 여러 번
    /// 띄우면 같은 프로파일 백업이 줄줄이 락에 걸린다(모듈 헤더 "놓친 실행").
    ///
    /// `stall`은 [`begin_tick`](Self::begin_tick)이 돌려준 값이다 — 틱 하나에 대해 한 번
    /// 계산해 그 틱의 모든 스케줄에 같은 값을 쓴다.
    fn evaluate(
        &mut self,
        schedule: &Schedule,
        now: DateTime<Utc>,
        stall: chrono::TimeDelta,
    ) -> Due {
        let cursor = *self.cursors.entry(schedule.id).or_insert(now);

        let mut latest: Option<DateTime<Utc>> = None;
        let mut passed = 0usize;
        let mut probe = cursor;
        // 지나간 후보를 모두 삼킨다. 상한은 `missed_since`와 같은 값을 쓴다 — 매분 스케줄이
        // 오래 밀린 경우에도 이 루프가 유한하다.
        while passed < schedules::MISSED_SCAN_MAX {
            match schedule.expr.next_after(probe) {
                Some(next) if next <= now => {
                    latest = Some(next);
                    probe = next;
                    passed += 1;
                }
                _ => break,
            }
        }

        let Some(at) = latest else {
            return Due::NotYet;
        };
        // 판정과 무관하게 커서를 전진시킨다 — 건너뛴 발화가 매 틱마다 다시 후보가 되면
        // 로그가 30초마다 같은 줄로 찬다(모듈 헤더 "중복 실행 방어" 2번).
        self.cursors.insert(schedule.id, at);
        let coalesced = passed - 1;
        if stall > LATE_FIRE_LIMIT {
            Due::TooLate { at, coalesced }
        } else {
            Due::Fire { at, coalesced }
        }
    }

    /// 사라진 스케줄의 커서를 정리한다 — 삭제·재생성이 반복되어도 메모리가 자라지 않게.
    fn retain_known(&mut self, schedules: &[Schedule]) {
        self.cursors
            .retain(|id, _| schedules.iter().any(|s| s.id == *id));
    }

    /// 다음으로 깨어날 시각까지의 대기 시간.
    fn sleep_until_next(&self, schedules: &[Schedule], now: DateTime<Utc>) -> Duration {
        let earliest = schedules
            .iter()
            .filter(|s| s.enabled)
            .filter_map(|s| {
                let cursor = self.cursors.get(&s.id).copied().unwrap_or(now);
                s.expr.next_after(cursor)
            })
            .min();
        let wait = match earliest {
            Some(next) => (next - now)
                .to_std()
                .unwrap_or(SCHEDULER_TICK_MIN)
                .min(SCHEDULER_TICK_MAX),
            // 활성 스케줄이 없으면 상한만큼 잔다 — 새 스케줄이 등록되면 그때 보인다.
            None => SCHEDULER_TICK_MAX,
        };
        wait.max(SCHEDULER_TICK_MIN)
    }
}

// ---------------------------------------------------------------------------
// 기동 진입점
// ---------------------------------------------------------------------------

/// 기동 경로에서 부르는 진입점 — 스케줄을 로드해 상태를 알리고 루프를 띄운다.
///
/// [`crate::web::reattach::run_at_startup`]과 같은 자리·같은 계약이다: **실패해도 반환한다**
/// (에러 타입이 없다). 스케줄 파일이 깨졌다고 콘솔이 뜨지 못하면 운영자는 그것을 고칠 화면
/// 조차 열 수 없다(모듈 헤더 "fail-loud").
///
/// 이 함수가 하는 일은 네 가지다:
/// 1. 스케줄 파일 로드 결과를 로그로 남긴다(손상이면 `error!`).
/// 2. 서버가 꺼져 있던 동안 **놓친 실행**을 스케줄별로 세어 알린다(실행하지는 않는다).
/// 3. 외부 crontab의 `x-backup` 항목을 감지해 경고한다(t30).
/// 4. 스케줄러 루프를 [`tokio::spawn`]으로 띄우고 즉시 반환한다.
///
/// 반환 전에 4번까지 끝나므로, 이 함수가 돌아온 시점부터 스케줄은 실제로 동작한다.
pub async fn run_at_startup(ctx: &Arc<ServeConfig>) {
    let store = schedules::shared(&ctx.state_dir);
    let report = store.report();
    report.log(store.path());

    let now = Utc::now();
    let all = store.list();
    report_missed_runs(&all, now);

    let enabled = all.iter().filter(|s| s.enabled).count();
    if enabled > 0 {
        tracing::info!(
            enabled,
            total = all.len(),
            next_run = ?next_run_across(&all, now),
            "내장 스케줄러를 시작합니다 — cron 표현식은 UTC 기준으로 해석됩니다"
        );
    }

    detect_crontab(ctx.jobs.secret_registry()).await.log();

    let ctx = Arc::clone(ctx);
    tokio::spawn(async move { scheduler_loop(ctx, store).await });
}

/// 기동 배너에 한 줄로 넣을 요약 — `server.rs`의 `print_banner`가 쓸 수 있게 노출한다.
///
/// 배선은 리더의 몫이다(이 태스크는 `server.rs`를 건드리지 않는다). 손상 상태를 **배너에
/// 반드시 실어야** 하는 이유: 기동 로그는 스크롤되어 사라지지만 배너는 운영자가 서버를 띄운
/// 터미널 화면에 남는다. 스케줄이 비어 있다는 사실이 눈에 보이는 마지막 자리다.
pub fn banner_line(state_dir: &std::path::Path, now: DateTime<Utc>) -> String {
    let store = schedules::shared(state_dir);
    let report = store.report();
    let all = store.list();
    let enabled = all.iter().filter(|s| s.enabled).count();
    if report.degraded() {
        return format!(
            "schedules: DEGRADED — {enabled} usable (스케줄 정의 파일을 온전히 읽지 못했습니다 \
             — 예약된 백업이 멈출 수 있습니다. /schedule 화면을 확인하세요)"
        );
    }
    match next_run_across(&all, now) {
        Some(next) => format!(
            "schedules: {enabled} enabled, next {} (UTC)",
            next.format("%Y-%m-%d %H:%M")
        ),
        None if all.is_empty() => "schedules: none".to_string(),
        None => format!("schedules: {} registered, none enabled", all.len()),
    }
}

/// 활성 스케줄 전체에서 가장 이른 다음 실행 시각.
fn next_run_across(all: &[Schedule], now: DateTime<Utc>) -> Option<DateTime<Utc>> {
    all.iter().filter_map(|s| s.next_run(now)).min()
}

/// 서버가 꺼져 있던 동안 놓친 실행을 스케줄별로 세어 알린다 — **실행하지는 않는다**
/// (모듈 헤더 "놓친 실행").
fn report_missed_runs(all: &[Schedule], now: DateTime<Utc>) {
    for schedule in all.iter().filter(|s| s.enabled) {
        let baseline = schedule.missed_baseline();
        let missed = schedule.missed_since(baseline, now);
        if missed == 0 {
            continue;
        }
        tracing::warn!(
            schedule = %schedule.id.short(),
            profile = %schedule.profile,
            expr = %schedule.expr,
            missed,
            capped = missed >= schedules::MISSED_SCAN_MAX,
            since = %baseline.to_rfc3339(),
            "콘솔이 꺼져 있던 동안 예약된 백업을 놓쳤습니다 — 지나간 실행은 따라잡지 \
             않습니다(재기동 직후 여러 백업이 몰려 서로 락에 걸리는 것을 막기 위함입니다). \
             그 기간의 백업이 필요하면 화면에서 직접 실행하세요"
        );
    }
}

// ---------------------------------------------------------------------------
// 루프
// ---------------------------------------------------------------------------

/// 스케줄러 본체 — 깨어나서 발화할 것을 발화하고 다시 잔다.
///
/// 스케줄마다 타이머를 두지 않고 **하나의 루프**로 가는 이유: 스케줄이 수정·삭제될 때마다
/// 해당 타이머를 취소하고 다시 세우는 상태 관리가 생기고, 그 상태가 어긋나면 "지운 스케줄이
/// 계속 돈다" 또는 "고친 스케줄이 옛 시각에 돈다"가 된다. 한 루프가 매번 현재 스냅샷을 다시
/// 읽으면 그 어긋남이 존재할 수 없다 — 스냅샷이 유일한 진실이다.
async fn scheduler_loop(ctx: Arc<ServeConfig>, store: Arc<ScheduleStore>) {
    let mut clock = ScheduleClock::default();
    loop {
        let now = Utc::now();
        let stall = clock.begin_tick(now);
        let all = store.list();
        clock.retain_known(&all);

        for schedule in all.iter().filter(|s| s.enabled) {
            match clock.evaluate(schedule, now, stall) {
                Due::NotYet => {}
                Due::TooLate { at, coalesced } => tracing::warn!(
                    schedule = %schedule.id.short(),
                    profile = %schedule.profile,
                    due_at = %at.to_rfc3339(),
                    stall_secs = stall.num_seconds(),
                    coalesced,
                    limit_secs = LATE_FIRE_LIMIT.num_seconds(),
                    "예약 시각이 지연 한도를 넘겨 발화하지 않습니다 — 서버가 사실상 멈춰 \
                     있었습니다(절전·과부하). 예고 없는 백업을 시작하지 않는 대신 이 사실을 \
                     남깁니다. 필요하면 화면에서 직접 실행하세요"
                ),
                Due::Fire { at, coalesced } => {
                    if coalesced > 0 {
                        tracing::warn!(
                            schedule = %schedule.id.short(),
                            coalesced,
                            "밀린 예약 실행을 가장 최근 하나로 합쳤습니다"
                        );
                    }
                    fire(&ctx, &store, schedule, at).await;
                }
            }
        }

        tokio::time::sleep(clock.sleep_until_next(&all, Utc::now())).await;
    }
}

/// 스케줄 하나를 발화한다 — 사전 확인 → spawn → 발화 기록.
///
/// 실패를 전파하지 않는다(반환값이 없다). 루프는 스케줄 하나의 실패로 멈추지 않아야 한다 —
/// 멈추면 **나머지 모든 스케줄의 백업이 함께 조용히 멈춘다.** 이 모듈이 막으려는 실패의
/// 정확한 형태다. 그래서 모든 실패는 로그로 남기고 다음 스케줄로 넘어간다.
async fn fire(
    ctx: &Arc<ServeConfig>,
    store: &Arc<ScheduleStore>,
    schedule: &Schedule,
    due_at: DateTime<Utc>,
) {
    // 1차 방어 — 같은 프로파일이 이미 돌고 있으면 건너뛴다(모듈 헤더 "중복 실행 방어").
    if let Some(running) = running_job_for_profile(&ctx.state_dir, &schedule.profile).await {
        tracing::info!(
            schedule = %schedule.id.short(),
            profile = %schedule.profile,
            running_job = %running,
            due_at = %due_at.to_rfc3339(),
            "이 프로파일의 백업이 아직 돌고 있어 예약 실행을 건너뜁니다 — 실패가 아닙니다. \
             주기가 백업 소요 시간보다 짧다면 표현식을 늘리세요"
        );
        return;
    }

    // 프로파일명은 저장 시점과 로드 시점 양쪽에서 이미 검증됐지만(`StoredSchedule::into_domain`),
    // argv가 되는 값이므로 여기서도 문을 지난다 — 검증을 한 번 더 하는 비용이 없다.
    let profile = match ProfileName::parse(&schedule.profile, ctx.lang) {
        Ok(profile) => profile,
        Err(e) => {
            tracing::error!(
                schedule = %schedule.id.short(),
                error = %e,
                "스케줄의 프로파일명이 유효하지 않아 실행할 수 없습니다 — 이 스케줄은 \
                 영구히 돌지 않습니다. 화면에서 수정하세요"
            );
            return;
        }
    };

    let spec = JobSpec::new(JobCommand::Backup, ctx.lang)
        .with_profile(profile)
        .with_backup_type(schedule.kind.backup_type());

    // 잡 실행 경로는 웹 화면과 **완전히 동일하다** — 감사 기록(요청·완료 두 줄)·잡 이력·
    // 로그 적재·SSE 발행이 전부 그 함수 안에서 배선된다. actor만 다르다.
    let job_id =
        match crate::web::routes::backup::spawn_tracked_job(ctx, spec, SCHEDULER_ACTOR).await {
            Ok(id) => {
                tracing::info!(
                    schedule = %schedule.id.short(),
                    profile = %schedule.profile,
                    kind = schedule.kind.token(),
                    job_id = %id,
                    due_at = %due_at.to_rfc3339(),
                    "예약된 백업을 시작했습니다"
                );
                Some(id)
            }
            Err(e) => {
                tracing::error!(
                    schedule = %schedule.id.short(),
                    profile = %schedule.profile,
                    error = %e,
                    "예약된 백업을 시작하지 못했습니다 — 다음 예약 시각에 다시 시도합니다"
                );
                None
            }
        };

    // 발화 기록은 **실패했더라도** 남긴다. `last_run_at`이 갱신되지 않으면 다음 기동에서
    // "놓친 실행"이 과대 보고되고(실제로는 시도했다), 운영자가 gap의 원인을 잘못 짚는다.
    // 잡 id는 성공했을 때만 얹는다(없는 잡으로 가는 링크를 만들지 않는다).
    if let Err(e) = store.record_run(schedule.id, due_at, job_id).await {
        tracing::error!(
            schedule = %schedule.id.short(),
            error = %e,
            "예약 실행 기록을 저장하지 못했습니다 — 다음 기동에서 놓친 실행이 실제보다 많게 \
             보고될 수 있습니다(실행 자체는 위 로그가 사실입니다)"
        );
    }
}

/// 같은 프로파일의 실행 중 잡이 있으면 그 id를 돌려준다.
///
/// [`crate::web::routes::backup`]에 같은 판단의 비공개 함수가 있다. 재사용하지 않은 이유는
/// 그것이 비공개이고 그 파일이 이 태스크의 소유가 아니라는 것뿐이다(공용화는 후속 정리
/// 대상이다). 읽기 전용이므로 [`JobStore::attach`]로 붙는다 — 스케줄러의 사전 확인이
/// 디렉터리를 만들지 않는다.
async fn running_job_for_profile(
    state_dir: &std::path::Path,
    profile: &str,
) -> Option<crate::web::state::jobs::JobId> {
    JobStore::attach(state_dir)
        .list()
        .await
        .entries
        .into_iter()
        .find(|entry| entry.is_running() && entry.profile.as_deref() == Some(profile))
        .map(|entry| entry.id)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::web::schedule::cron::CronExpr;
    use crate::web::state::schedules::ScheduleKind;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .expect("테스트 시각")
            .with_timezone(&Utc)
    }

    /// 틱 하나를 돌린다 — `begin_tick`으로 정체 시간을 재고 그 값으로 판정한다
    /// (프로덕션 루프와 같은 순서).
    fn tick(clock: &mut ScheduleClock, schedule: &Schedule, now: DateTime<Utc>) -> Due {
        let stall = clock.begin_tick(now);
        clock.evaluate(schedule, now, stall)
    }

    fn schedule(expr: &str) -> Schedule {
        Schedule {
            id: ScheduleId::generate(),
            profile: "prod".to_string(),
            expr: CronExpr::parse(expr, Lang::En).expect("표현식"),
            kind: ScheduleKind::Full,
            enabled: true,
            created_at: at("2026-01-01T00:00:00Z"),
            last_run_at: None,
            last_job_id: None,
        }
    }

    /// 새로 본 스케줄은 **그 순간부터** 평가된다 — 등록·기동 전의 시각은 후보가 아니다
    /// (놓친 실행 건너뛰기가 초기값에서 나온다는 성질).
    #[test]
    fn a_newly_seen_schedule_never_fires_for_the_past() {
        let mut clock = ScheduleClock::default();
        let daily = schedule("0 3 * * *");
        // 등록 시각이 반 년 전이어도, 처음 보는 순간에는 발화하지 않는다.
        let now = at("2026-07-25T12:00:00Z");
        assert_eq!(tick(&mut clock, &daily, now), Due::NotYet);
        // 다음 03:00을 지나야 비로소 발화한다. 정상 운영의 틱 간격(수십 초)을 흉내내
        // 마지막 틱을 목표 시각 바로 앞에 둔다 — 여기서 몇 시간을 건너뛰면 그것은 이
        // 테스트가 보려는 것(과거를 발화하지 않는다)이 아니라 루프 정체 판정
        // ([`LATE_FIRE_LIMIT`])을 건드리게 된다.
        assert_eq!(
            tick(&mut clock, &daily, at("2026-07-26T02:59:40Z")),
            Due::NotYet
        );
        assert_eq!(
            tick(&mut clock, &daily, at("2026-07-26T03:00:00Z")),
            Due::Fire {
                at: at("2026-07-26T03:00:00Z"),
                coalesced: 0
            }
        );
    }

    /// 같은 시각을 두 번 발화하지 않는다 — 커서가 전진하므로 같은 틱이 반복돼도 조용하다.
    #[test]
    fn the_same_occurrence_never_fires_twice() {
        let mut clock = ScheduleClock::default();
        let minutely = schedule("* * * * *");
        let start = at("2026-07-25T10:00:00Z");
        assert_eq!(tick(&mut clock, &minutely, start), Due::NotYet);

        let fired = at("2026-07-25T10:01:00Z");
        assert_eq!(
            tick(&mut clock, &minutely, fired),
            Due::Fire {
                at: fired,
                coalesced: 0
            }
        );
        // 같은 벽시계로 다시 평가해도 발화하지 않는다.
        assert_eq!(tick(&mut clock, &minutely, fired), Due::NotYet);
        assert_eq!(
            tick(&mut clock, &minutely, at("2026-07-25T10:01:30Z")),
            Due::NotYet
        );
    }

    /// 밀린 발화는 **하나로 합쳐진다** — 지연이 한도 안이면 한 번만 뜬다.
    #[test]
    fn backlog_is_coalesced_into_one_fire() {
        let mut clock = ScheduleClock::default();
        let minutely = schedule("* * * * *");
        let start = at("2026-07-25T10:00:00Z");
        tick(&mut clock, &minutely, start);

        // 40분 뒤에 깨어났다(지연 한도 1시간 안).
        let now = at("2026-07-25T10:40:00Z");
        let Due::Fire {
            at: fired,
            coalesced,
        } = tick(&mut clock, &minutely, now)
        else {
            panic!("발화해야 함");
        };
        assert_eq!(fired, now, "가장 최근 후보로 합쳐지지 않았다");
        assert_eq!(coalesced, 39, "합쳐진 개수가 어긋난다");
        // 합친 뒤에는 곧바로 다시 발화하지 않는다.
        assert_eq!(tick(&mut clock, &minutely, now), Due::NotYet);
    }

    /// 지연이 [`LATE_FIRE_LIMIT`]를 넘으면 발화하지 않는다 — 예고 없는 프로덕션 백업을
    /// 만들지 않는다. 그래도 커서는 전진해 다음 정상 시각부터 다시 돈다.
    #[test]
    fn stale_backlog_is_reported_but_not_fired() {
        let mut clock = ScheduleClock::default();
        let hourly = schedule("0 * * * *");
        tick(&mut clock, &hourly, at("2026-07-25T00:30:00Z"));

        // 사흘 절전 후 깨어났다.
        let now = at("2026-07-28T00:30:00Z");
        let Due::TooLate { at: due, coalesced } = tick(&mut clock, &hourly, now) else {
            panic!("지연 한도를 넘겼으므로 발화하지 않아야 함");
        };
        assert_eq!(due, at("2026-07-28T00:00:00Z"));
        assert!(coalesced > 0);

        // 커서가 전진했으므로 다음 정상 시각에는 정상 발화한다.
        assert_eq!(
            tick(&mut clock, &hourly, at("2026-07-28T01:00:00Z")),
            Due::Fire {
                at: at("2026-07-28T01:00:00Z"),
                coalesced: 0
            }
        );
    }

    /// **DST 전환 구간에서 중복도 누락도 없다.**
    ///
    /// 커서 기반 평가가 [`cron`]의 UTC 계산 위에 있으므로, 벽시계가 전환을 지나가도 하루에
    /// 정확히 한 번씩만 발화한다. 가을 전환(같은 로컬 시각이 두 번 오는 구간)을 1분 간격으로
    /// 관통시켜 발화 시각 집합이 정확히 기대한 날짜 목록과 같은지 본다.
    #[test]
    fn dst_transitions_fire_exactly_once_per_day() {
        // America/New_York 2026 가을 전환 = 2026-11-01T06:00:00Z.
        let mut clock = ScheduleClock::default();
        let daily = schedule("30 1 * * *");
        let mut cursor = at("2026-10-31T00:00:00Z");
        let end = at("2026-11-03T00:00:00Z");
        let mut fired = Vec::new();
        while cursor < end {
            if let Due::Fire { at: t, coalesced } = tick(&mut clock, &daily, cursor) {
                assert_eq!(coalesced, 0, "1분 간격 평가에서 발화가 밀렸다");
                fired.push(t);
            }
            cursor += chrono::TimeDelta::minutes(1);
        }
        assert_eq!(
            fired,
            vec![
                at("2026-10-31T01:30:00Z"),
                at("2026-11-01T01:30:00Z"),
                at("2026-11-02T01:30:00Z"),
            ],
            "가을 전환 구간에서 중복 또는 누락이 발생했다"
        );

        // 봄 전환 = 2026-03-08T07:00:00Z. 존재하지 않는 로컬 시각(02:30 EST)에 해당하는
        // 스케줄도 UTC에서는 매일 정확히 한 번이다.
        let mut clock = ScheduleClock::default();
        let daily = schedule("30 2 * * *");
        let mut cursor = at("2026-03-07T00:00:00Z");
        let end = at("2026-03-10T00:00:00Z");
        let mut fired = Vec::new();
        while cursor < end {
            if let Due::Fire { at: t, .. } = tick(&mut clock, &daily, cursor) {
                fired.push(t);
            }
            cursor += chrono::TimeDelta::minutes(1);
        }
        assert_eq!(
            fired,
            vec![
                at("2026-03-07T02:30:00Z"),
                at("2026-03-08T02:30:00Z"),
                at("2026-03-09T02:30:00Z"),
            ],
            "봄 전환 구간에서 실행이 누락됐다"
        );
    }

    /// 사라진 스케줄의 커서는 정리된다.
    #[test]
    fn cursors_of_deleted_schedules_are_dropped() {
        let mut clock = ScheduleClock::default();
        let a = schedule("0 3 * * *");
        let b = schedule("0 4 * * *");
        tick(&mut clock, &a, at("2026-07-25T00:00:00Z"));
        tick(&mut clock, &b, at("2026-07-25T00:00:00Z"));
        assert_eq!(clock.cursors.len(), 2);
        clock.retain_known(std::slice::from_ref(&a));
        assert_eq!(clock.cursors.len(), 1);
        assert!(clock.cursors.contains_key(&a.id));
    }

    /// 대기 시간은 다음 실행까지지만 상한·하한에 묶인다.
    #[test]
    fn sleep_is_bounded_on_both_ends() {
        let clock = ScheduleClock::default();
        let now = at("2026-07-25T10:00:00Z");

        // 스케줄이 없으면 상한만큼 잔다.
        assert_eq!(clock.sleep_until_next(&[], now), SCHEDULER_TICK_MAX);

        // 먼 미래여도 상한을 넘지 않는다(모노토닉 시계 문제 — 상수 doc 참조).
        let daily = schedule("0 3 * * *");
        assert_eq!(
            clock.sleep_until_next(std::slice::from_ref(&daily), now),
            SCHEDULER_TICK_MAX
        );

        // 곧이면 그 시각까지만 잔다.
        let soon = schedule("* * * * *");
        let wait = clock.sleep_until_next(std::slice::from_ref(&soon), at("2026-07-25T10:00:50Z"));
        assert!(
            wait <= Duration::from_secs(10) && wait >= SCHEDULER_TICK_MIN,
            "대기가 이상하다: {wait:?}"
        );

        // 비활성 스케줄은 대기 계산에 들어가지 않는다.
        let mut disabled = schedule("* * * * *");
        disabled.enabled = false;
        assert_eq!(
            clock.sleep_until_next(std::slice::from_ref(&disabled), now),
            SCHEDULER_TICK_MAX
        );
    }

    /// crontab 본문에서 우리 항목만 골라낸다 — 주석·빈 줄·무관한 항목은 무시한다.
    #[test]
    fn crontab_parsing_finds_only_real_entries() {
        let registry = SecretRegistry::default();
        let text = "\
# x-backup 관련 메모(주석이므로 무시)
MAILTO=ops@example.com

0 3 * * * /usr/local/bin/x-backup backup --profile prod
30 4 * * * /usr/bin/pg_dump mydb > /backup/db.sql
  0 5 * * * x-backup verify --profile prod
";
        let entries = collect_crontab_entries(text, &registry);
        assert_eq!(entries.len(), 2, "항목 수가 어긋난다: {entries:?}");
        assert!(entries[0].contains("--profile prod"));
        assert!(
            entries[1].starts_with("0 5 * * *"),
            "앞 공백이 정리되지 않았다"
        );
        assert!(
            !entries.iter().any(|e| e.contains("pg_dump")),
            "무관한 항목이 섞였다"
        );
        assert!(
            !entries.iter().any(|e| e.starts_with('#')),
            "주석이 항목으로 잡혔다"
        );
    }

    /// **로그 경로는 실행이 아니다.** 이름이 스치기만 한 줄에 경고가 뜨면, 멀쩡한 crontab을
    /// 쓰는 운영자가 매 기동마다 같은 경고를 보게 되고 — 그러면 진짜 충돌이 왔을 때 아무도
    /// 읽지 않는다. 이 경고의 가치는 드물게 뜨는 데 있다.
    #[test]
    fn a_line_that_merely_mentions_the_name_is_not_a_run() {
        let registry = SecretRegistry::default();
        let text = "\
0 3 * * * /opt/other-tool run >> /var/log/x-backup.log 2>&1
30 4 * * * /usr/bin/rotate /var/lib/x-backup-old
15 5 * * * /usr/bin/find /srv -name 'x-backup.*' -delete
";
        let entries = collect_crontab_entries(text, &registry);
        assert!(
            entries.is_empty(),
            "실행이 아닌 줄에 경고가 뜬다 — 늑대 소년이 된다: {entries:?}"
        );
    }

    /// 셸 구두점이 붙어도 실행은 실행이다(위 테스트가 과하게 좁아지지 않았는지 확인).
    #[test]
    fn shell_punctuation_does_not_hide_a_real_run() {
        let registry = SecretRegistry::default();
        let text = "\
0 3 * * * cd /srv && /srv/x-backup backup --profile prod; echo done
0 4 * * * (x-backup verify)
";
        assert_eq!(
            collect_crontab_entries(text, &registry).len(),
            2,
            "구두점이 붙은 진짜 실행을 놓쳤다"
        );
    }

    /// **config가 모르는 자격증명도 지운다.**
    ///
    /// 이 테스트가 잡는 회귀: 마스킹을 `SecretRegistry`에만 맡기면, 레지스트리는 config가
    /// 등록한 값만 알기 때문에 crontab 줄에 인라인으로 박힌 **미등록** 자격증명이 그대로
    /// 스케줄 화면에 실린다. crontab 항목은 콘솔보다 먼저 있었거나 다른 계정을 쓰는 경우가
    /// 많아, 그 값이 config에 없는 것이 오히려 흔하다.
    #[test]
    fn an_inline_credential_the_config_never_saw_is_still_redacted() {
        let registry = SecretRegistry::default(); // 아무것도 등록되지 않았다
        let text = "0 3 * * * XB_URI=mongodb://u:neverInConfig@h/db x-backup backup\n";
        let entries = collect_crontab_entries(text, &registry);
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].contains("neverInConfig"),
            "레지스트리가 모르는 자격증명이 화면으로 나간다: {}",
            entries[0]
        );
        assert!(
            entries[0].contains("XB_URI=<redacted>"),
            "env 이름까지 지우면 운영자가 crontab에서 그 줄을 못 찾는다: {}",
            entries[0]
        );
    }

    /// 값이 있어야 뜻이 통하는 플래그는 가리지 않는다 — 자격증명이 사는 자리가 아니다.
    #[test]
    fn flag_values_survive_redaction() {
        let registry = SecretRegistry::default();
        let text = "0 3 * * * x-backup backup --profile=prod --config=/etc/xb.toml\n";
        let entries = collect_crontab_entries(text, &registry);
        assert!(entries[0].contains("--profile=prod"), "{}", entries[0]);
        assert!(
            entries[0].contains("--config=/etc/xb.toml"),
            "{}",
            entries[0]
        );
    }

    /// crontab 항목의 시크릿은 화면·로그에 나가기 전에 지워지고, 긴 줄은 잘린다.
    #[test]
    fn crontab_entries_are_masked_and_truncated() {
        let mut registry = SecretRegistry::default();
        registry.register("s3cr3t-p4ssw0rd");
        let text = format!(
            "0 3 * * * XB_URI=mongodb://u:s3cr3t-p4ssw0rd@h/db x-backup backup --profile prod {}\n",
            "x".repeat(CRONTAB_LINE_MAX)
        );
        let entries = collect_crontab_entries(&text, &registry);
        assert_eq!(entries.len(), 1);
        assert!(
            !entries[0].contains("s3cr3t-p4ssw0rd"),
            "시크릿 원문이 새어 나왔다: {}",
            entries[0]
        );
        assert!(
            entries[0].chars().count() <= CRONTAB_LINE_MAX + 1,
            "길이 제한이 적용되지 않았다: {}자",
            entries[0].chars().count()
        );
    }

    /// 항목 개수 상한이 지켜진다.
    #[test]
    fn crontab_entry_count_is_capped() {
        let registry = SecretRegistry::default();
        let text = (0..CRONTAB_ENTRIES_MAX * 2)
            .map(|i| format!("{i} 3 * * * x-backup backup --profile p{i}\n"))
            .collect::<String>();
        assert_eq!(
            collect_crontab_entries(&text, &registry).len(),
            CRONTAB_ENTRIES_MAX
        );
    }

    /// **crontab이 없거나 실패해도** 조용히 넘어간다(기동을 막지 않는다).
    ///
    /// 실제 `crontab` 바이너리의 존재를 전제할 수 없으므로(CI 컨테이너에는 대개 없다) 두
    /// 결과 중 무엇이 나와도 통과해야 한다 — 확인하는 것은 "패닉하지 않고, 충돌이 없으면
    /// 조용하다"는 성질이다.
    #[tokio::test]
    async fn crontab_probe_never_blocks_startup() {
        let scan = detect_crontab(&SecretRegistry::default()).await;
        // 조회 실패든 성공이든 로그가 패닉하지 않는다.
        scan.log();
        if scan.unavailable.is_some() {
            assert!(
                scan.entries.is_empty(),
                "조회 실패인데 항목이 있다: {scan:?}"
            );
            assert!(!scan.conflicts());
        }
    }

    /// 배너 한 줄은 상태를 정직하게 말한다 — 손상이면 그 사실을 숨기지 않는다.
    #[test]
    fn banner_line_states_the_truth() {
        let tmp = tempfile::tempdir().unwrap();
        // 스케줄 없음.
        let line = banner_line(tmp.path(), Utc::now());
        assert_eq!(line, "schedules: none");

        // 손상된 파일 — 별도 디렉터리를 쓴다(`shared`가 디렉터리별로 한 번만 로드한다).
        let broken = tempfile::tempdir().unwrap();
        std::fs::write(
            broken
                .path()
                .join(crate::web::state::schedules::SCHEDULES_FILE_NAME),
            b"{ not json",
        )
        .unwrap();
        let line = banner_line(broken.path(), Utc::now());
        assert!(line.contains("DEGRADED"), "손상이 배너에 없다: {line}");
        assert!(
            line.contains("/schedule"),
            "고칠 곳을 알려주지 않는다: {line}"
        );
    }
}
