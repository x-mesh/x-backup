//! 스케줄 정의 — **캐시가 아니라 진짜 상태다.**
//!
//! ## 이 파일이 [`super::jobs`]와 근본적으로 다른 이유
//! [`super`] 모듈 헤더는 "여기 있는 모든 것은 manifest에서 재구성 가능한 캐시다"라는 규칙을
//! 세웠다. 잡 이력은 실제로 그렇다 — 상태 파일이 날아가도 백업은 멀쩡하고, 잃는 것은 "누가
//! 언제 눌렀는지"뿐이다. 그래서 [`super::jobs::JobStore::open`]은 실패해도 서버를 띄운다.
//!
//! **스케줄은 그 규칙의 예외다.** 어디에도 재구성할 원본이 없다:
//!
//! - destination의 manifest에는 "무슨 백업이 있었나"만 있고 "앞으로 언제 돌 것인가"는 없다.
//! - 외부 cron을 대체하는 것이 이 기능의 존재 이유이므로, crontab에도 사본이 없다.
//!
//! 그래서 스케줄 정의가 날아가면 **백업이 조용히 멈춘다.** 이 도구에서 가장 나쁜 실패다 —
//! 운영자는 백업이 돌고 있다고 믿고, 그 믿음이 깨지는 순간은 복구가 필요해진 순간이다.
//! 이 파일의 모든 설계 결정이 그 한 문장에서 나온다.
//!
//! ## 왜 append-only NDJSON이 아니라 단일 JSON + 원자적 교체인가
//! [`super::jobs`]와 [`crate::web::audit`]는 append-only NDJSON이다. 그 형식이 맞는 이유는
//! 둘 다 **사건의 나열**이기 때문이다 — 이미 일어난 일은 수정되지 않는다.
//!
//! 스케줄은 사건이 아니라 **현재 상태**이고 수정·삭제가 정상 동작이다. append-only로 CRUD를
//! 표현하려면 "생성/수정/삭제 사건을 쌓고 읽을 때 접는" 이벤트 소싱이 되는데, 그 대가가
//! 스케줄 5개짜리 파일에 대해 전혀 남지 않는다:
//!
//! - 읽기가 전체 파일 스캔 + 폴딩이 된다(파일은 영원히 자란다 — 스케줄 하나를 30번
//!   수정하면 30줄이다).
//! - **잘린 마지막 줄의 의미가 위험해진다.** 이력에서 잘린 줄은 "로그 한 줄을 잃음"이지만,
//!   여기서 잘린 줄이 삭제 사건이면 "지웠는데 다시 살아난 스케줄"이 된다.
//! - "지금 무엇이 등록되어 있는가"를 사람이 파일을 열어 확인할 수 없다.
//!
//! 그래서 `<state>/schedules.json` 하나에 전체 상태를 담고 **원자적 교체**(tmp 같은 디렉터리 →
//! fsync → rename → 디렉터리 fsync)로 갱신한다. t27의 `config_write.rs`가 config에 쓴 것과
//! 같은 순서이고, 근거도 같다(그 파일의 `write_atomic` doc이 각 단계를 길게 설명한다 — 이
//! 파일은 그것을 읽고 같은 순서를 따르되 코드를 재사용하지는 않았다. 그 함수는 비공개이고
//! t28 소유 파일이다).
//!
//! ## 직전본을 남긴다 — 한 세대만
//! 교체 전에 현재 파일을 [`SCHEDULES_PREV_FILE_NAME`]으로 복사한다. 근거: 이 파일의 유실은
//! "백업이 조용히 멈춤"이고, 직전본 한 세대의 비용은 수백 바이트다. 두 세대 이상은 두지
//! 않는다 — 세대가 늘면 "어느 것이 맞는 것인가"를 운영자가 판단해야 하고, 그 판단은 직전본
//! 하나일 때만 명확하다.
//!
//! **손상된 본문을 직전본에 밀어 넣지 않는다.** 로드가 손상을 만났으면([`LoadReport::degraded`])
//! 그 다음 쓰기는 직전본 갱신을 건너뛴다 — 그러지 않으면 마지막으로 남은 온전한 사본을
//! 손상된 바이트로 덮어쓴다.
//!
//! ## 손상 시 정책 — fail-closed가 아니라 **fail-loud**
//! 감사 로그는 fail-closed다(못 열면 기동 거부). 스케줄은 그렇게 하지 않는다:
//!
//! > **기동은 한다. 대신 로그·기동 배너·화면에서 크게 알린다.**
//!
//! 근거: 손상을 고칠 수단이 콘솔 그 자체다. 스케줄 파일이 깨졌다고 콘솔이 뜨지 않으면
//! 운영자는 **고칠 화면조차 못 열고** JSON을 손으로 편집해야 한다. 그건 손상을 하나 더
//! 만드는 길이다. 그리고 스케줄이 깨진 서버도 나머지 기능(수동 백업·복구·점검)은 전부
//! 정상이다 — 그 전부를 함께 잠글 이유가 없다.
//!
//! 대신 "조용히 빈 목록으로 진행"은 절대 하지 않는다. 그게 정확히 이 파일이 막으려는 실패다.
//! 세 곳에서 동시에 알린다: `tracing::error!` · 기동 배너 · 스케줄 화면 최상단 배너.
//!
//! ## 손상된 파일을 **이동하지 않고 복사한다**
//! 손상을 만나면 [`SCHEDULES_CORRUPT_FILE_NAME`]으로 **복사**하고 원본은 그 자리에 둔다.
//! rename으로 치우지 않는 이유가 중요하다: 치워 버리면 `schedules.json`이 없어져 다음
//! 기동에서 **"신규 설치"로 보인다** — 손상 경고가 사라지고 빈 목록이 정상처럼 보인다. 그게
//! 이 모듈이 막으려는 바로 그 실패다. 복사만 하면 복구가 **멱등**해진다: 다음 기동도 같은
//! 손상을 다시 감지해 같은 경고를 낸다. 온전한 내용이 파일에 실제로 쓰이는 시점은 운영자가
//! 화면에서 무언가를 저장하는 순간이다.
//!
//! ## 쓰기 경로는 인스턴스를 공유해야 한다
//! [`super::jobs`]와 같은 판단이다. 여기서는 이유가 하나 더 있다 — 이 저장소는 파일뿐 아니라
//! **메모리 스냅샷**([`ScheduleStore::list`])도 들고 있고, 그 스냅샷이 스케줄러 루프가 보는
//! 유일한 창이다. 인스턴스가 두 개면 한쪽에서 만든 스케줄이 다른 쪽 루프에 보이지 않는다.
//! 그래서 [`shared`]로만 얻는다.
//!
//! ## 손으로 편집한 파일은 재기동해야 반영된다
//! 로드 이후의 진실은 메모리 스냅샷이다(루프가 30초마다 파일을 다시 읽지 않는다 — 읽으면
//! 손상 감지 로직이 요청 경로 밖에서 또 돌아야 하고, 그 실패를 알릴 자리가 없다). 운영자가
//! `schedules.json`을 직접 고쳤다면 콘솔을 재기동해야 한다. 정상 경로는 화면이다.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::web::audit::AuditReceipt;
use crate::web::job::args::ProfileName;
use crate::web::schedule::cron::CronExpr;
use crate::web::state::jobs::JobId;

/// 스케줄 정의 파일 이름(state 디렉터리 기준).
pub const SCHEDULES_FILE_NAME: &str = "schedules.json";

/// 직전본 파일 이름 — 교체 전에 현재 내용을 여기로 복사한다(모듈 헤더 "직전본").
pub const SCHEDULES_PREV_FILE_NAME: &str = "schedules.json.prev";

/// 손상된 본문을 보존하는 파일 이름. 슬롯이 하나뿐인 이유는 원본을 치우지 않아
/// 복구가 멱등이기 때문이다(모듈 헤더 "이동하지 않고 복사한다").
pub const SCHEDULES_CORRUPT_FILE_NAME: &str = "schedules.json.corrupt";

/// 저장 파일의 스키마 버전. 외부 도구가 읽는 문서이므로 함께 남긴다(t4·t14와 같은 판단).
pub const SCHEDULES_SCHEMA: u32 = 1;

/// 등록할 수 있는 스케줄 수의 상한.
///
/// 64의 근거: 프로파일 하나당 풀·증분 두 개를 두는 것이 일반적인 최대 형태이므로 32
/// 프로파일까지 감당한다. 상한이 필요한 이유는 두 가지다 — (1) 스케줄러 루프가 매 틱마다
/// 전체를 훑으므로 개수가 곧 틱 비용이고, (2) 상한이 없으면 폼을 반복 제출하는 사고 하나가
/// 상태 파일을 무한히 키운다.
pub const MAX_SCHEDULES: usize = 64;

/// 놓친 실행을 셀 때의 상한([`Schedule::missed_since`]).
///
/// 1000의 근거: 매분 스케줄이 약 17시간 멈춰 있던 분량이다. 그 이상은 개수를 정확히 아는
/// 것이 운영자에게 주는 정보가 없다("아주 많이 놓쳤다"는 사실이 전부다). 상한 없이 세면
/// 한 달 멈춘 매분 스케줄에서 4만 번 이상 반복한다.
pub const MISSED_SCAN_MAX: usize = 1_000;

// ---------------------------------------------------------------------------
// 식별자
// ---------------------------------------------------------------------------

/// 스케줄 하나의 식별자 — UUID v7.
///
/// [`JobId`]와 같은 설계다: 내부를 파싱된 [`Uuid`]로 들고 있어 밖에서 온 문자열이 검증
/// 없이 URL 경로나 조회 키가 되는 경로를 없앤다. 여기서는 파일명이 되지는 않지만(스케줄은
/// 파일 하나에 모여 있다) `POST /schedule/delete`의 대상 지정에 그대로 쓰인다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ScheduleId(Uuid);

impl ScheduleId {
    /// 새 id — UUID v7이라 사전순이 생성순이다(목록 정렬이 공짜다).
    #[must_use]
    pub fn generate() -> Self {
        Self(Uuid::now_v7())
    }

    /// 문자열을 id로 접는다. 폼에서 온 값이 전부 이 문을 지난다.
    pub fn parse(raw: &str) -> Result<Self> {
        Uuid::parse_str(raw).map(Self).map_err(|_| {
            XBackupError::Usage(format!(
                "스케줄 id '{raw}'는 UUID 형식이 아닙니다 — 화면의 링크가 잘못되었거나 \
                 손상된 스케줄 항목입니다."
            ))
        })
    }

    /// 표시용 단축형(앞 8자) — 표에서 id 열이 화면을 다 먹지 않게 한다.
    pub fn short(&self) -> String {
        self.0.as_hyphenated().to_string().chars().take(8).collect()
    }
}

impl std::fmt::Display for ScheduleId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.as_hyphenated())
    }
}

// ---------------------------------------------------------------------------
// 도메인 모델
// ---------------------------------------------------------------------------

/// 스케줄이 띄우는 백업의 종류.
///
/// **모르는 값을 관용적으로 접지 않는다.** [`super::jobs::LogStream`]은 모르는 스트림을
/// `Unknown`으로 접지만(로그 한 줄을 잃는 것보다 낫다), 여기서 모르는 종류를 임의로 접으면
/// **의도와 다른 백업이 돈다** — 증분을 원했는데 풀이 돌면 프로덕션 DB에 예상치 못한 부하가
/// 걸리고, 반대면 RPO가 조용히 어긋난다. 그래서 알 수 없는 값은 그 항목 전체를 실패시킨다
/// (그 항목만 실패하고 나머지 스케줄은 살아남는다 — [`ScheduleStore`] 로드 참조).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScheduleKind {
    /// 풀 백업(`--type full`).
    Full,
    /// 증분 백업(`--type incr`).
    Incr,
}

impl ScheduleKind {
    /// 폼 값·화면 표기에 쓰는 토큰. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
    pub fn token(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Incr => "incr",
        }
    }

    /// 폼 값을 접는다. 모르는 값은 거부한다(위 doc의 근거).
    pub fn parse(raw: &str) -> Result<Self> {
        match raw {
            "full" => Ok(Self::Full),
            "incr" => Ok(Self::Incr),
            other => Err(XBackupError::Usage(format!(
                "백업 종류 '{other}'를 알 수 없습니다 — `full` 또는 `incr`만 쓸 수 있습니다."
            ))),
        }
    }

    /// 잡 인자로 넘길 CLI 어휘.
    pub fn backup_type(self) -> crate::cli::args::BackupType {
        match self {
            Self::Full => crate::cli::args::BackupType::Full,
            Self::Incr => crate::cli::args::BackupType::Incr,
        }
    }
}

/// 스케줄 하나.
///
/// 디스크 표현이 아니다 — 저장 형식은 [`StoredSchedule`]이고 이 타입은 그것을 검증해 접은
/// 결과다([`super::jobs`]가 `JobRecord`와 `JobSummary`를 가른 것과 같은 이유). 그래서 이
/// 구조체의 `expr`은 **이미 파싱을 통과한** [`CronExpr`]이고, 화면이 "다음 실행 시각"을
/// 계산하려고 다시 파싱할 필요가 없다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    /// 스케줄 id.
    pub id: ScheduleId,
    /// 대상 프로파일. [`ProfileName`]을 통과한 값만 들어온다.
    pub profile: String,
    /// cron 표현식(**UTC 기준** — [`crate::web::schedule::cron`] 헤더).
    pub expr: CronExpr,
    /// 백업 종류.
    pub kind: ScheduleKind,
    /// 활성 여부. 끄면 타이머가 건너뛴다(삭제하지 않고 잠시 멈추는 정상 수단).
    pub enabled: bool,
    /// 등록 시각(UTC).
    pub created_at: DateTime<Utc>,
    /// 스케줄러가 이 스케줄을 마지막으로 발화한 시각. `None`이면 아직 한 번도 돈 적이 없다.
    ///
    /// 이 값이 **놓친 실행 판정의 기준선**이다(모듈 [`crate::web::schedule`] 헤더의 "놓친
    /// 실행" 참조) — 없으면 [`created_at`](Self::created_at)을 기준으로 본다.
    pub last_run_at: Option<DateTime<Utc>>,
    /// 마지막 발화가 만든 잡 id — 화면이 `/jobs/{id}` 링크를 그린다.
    pub last_job_id: Option<JobId>,
}

impl Schedule {
    /// `now` 이후의 다음 실행 시각. **비활성이면 `None`이다.**
    ///
    /// 비활성과 "앞으로 실행되지 않는 표현식"이 같은 `None`으로 접히는 것은 의도적이다 —
    /// 둘 다 "다음 실행이 없다"이고, 화면은 [`enabled`](Self::enabled)를 따로 보고 있어
    /// 두 경우를 구분해 표시할 수 있다([`crate::web::view::schedule`]).
    pub fn next_run(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        if !self.enabled {
            return None;
        }
        self.expr.next_after(now)
    }

    /// `since`(배타) ~ `now`(포함) 사이에 지나간 실행 시각의 개수 — 놓친 실행 수.
    ///
    /// [`MISSED_SCAN_MAX`]에서 멈춘다. 반환값이 그 값과 같으면 "그 이상"이라는 뜻이고,
    /// 화면은 `1000+`처럼 표기한다.
    pub fn missed_since(&self, since: DateTime<Utc>, now: DateTime<Utc>) -> usize {
        let mut cursor = since;
        let mut count = 0usize;
        while count < MISSED_SCAN_MAX {
            match self.expr.next_after(cursor) {
                Some(next) if next <= now => {
                    count += 1;
                    cursor = next;
                }
                _ => break,
            }
        }
        count
    }

    /// 놓친 실행 판정의 기준선 — 마지막 발화 시각, 없으면 등록 시각.
    pub fn missed_baseline(&self) -> DateTime<Utc> {
        self.last_run_at.unwrap_or(self.created_at)
    }
}

/// 검증을 통과한 생성·수정 입력.
///
/// 라우트 핸들러가 폼을 이 타입으로 접어서 넘긴다. 저장소가 문자열을 받지 않는 것이
/// 요점이다 — 프로파일명·표현식·종류의 검증이 저장 직전이 아니라 **경계에서** 끝난다.
#[derive(Debug, Clone)]
pub struct ScheduleDraft {
    /// 대상 프로파일.
    pub profile: ProfileName,
    /// cron 표현식(UTC).
    pub expr: CronExpr,
    /// 백업 종류.
    pub kind: ScheduleKind,
    /// 활성 여부.
    pub enabled: bool,
}

// ---------------------------------------------------------------------------
// 디스크 표현
// ---------------------------------------------------------------------------

/// 파일 전체의 모양.
///
/// `schedules`를 [`serde_json::Value`]로 받는 것이 이 구조체의 핵심이다. 강타입으로 받으면
/// **항목 하나가 깨졌을 때 파일 전체가 실패한다** — 스케줄 10개 중 1개의 표현식이 손으로
/// 편집되다 깨졌다고 나머지 9개의 백업까지 멈추면 안 된다. 항목별로 따로 접어 실패를
/// 세는 것이 [`super::jobs`]의 "손상은 삼키지 않고 센다"와 같은 규율이다.
#[derive(Debug, Deserialize)]
struct ScheduleFileIn {
    /// 스키마 버전. 이 빌드가 모르는 상위 버전이면 로드를 거부한다(아래 근거).
    schema: u32,
    schedules: Vec<serde_json::Value>,
}

/// 쓸 때의 파일 모양 — 항목이 이미 검증된 값이라 강타입으로 직렬화한다.
#[derive(Debug, Serialize)]
struct ScheduleFileOut<'a> {
    schema: u32,
    schedules: &'a [StoredSchedule],
}

/// 항목 하나의 저장 모양.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct StoredSchedule {
    id: String,
    profile: String,
    /// [`CronExpr`]의 serde 구현이 문자열 하나로 접는다 — 사람이 파일을 열어 읽을 수 있다.
    expr: CronExpr,
    kind: ScheduleKind,
    enabled: bool,
    created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_run_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    last_job_id: Option<String>,
}

impl StoredSchedule {
    /// 저장 표현을 도메인 값으로 접는다 — 여기서 모든 재검증이 일어난다.
    ///
    /// 프로파일명을 다시 [`ProfileName::parse`]에 통과시키는 이유: 이 파일은 사람이 편집할
    /// 수 있고, 프로파일명은 그대로 자식 프로세스의 argv가 된다. 저장할 때 검증했다는 것이
    /// 읽을 때 안전하다는 뜻이 아니다.
    ///
    /// **`lang`이 인자가 아닌 이유**: 여기서 나온 오류 문장은 화면에 닿지 않는다. 뷰는
    /// `LoadReport::fatal_error`를 `is_some()`으로만 보고(`view::schedule::degraded_banner`),
    /// 사유 문자열은 `tracing::error!`로만 나간다. 그래서 이 파일의 다른 로그 문구와 같은
    /// 언어를 쓴다 — 사용자 언어를 저장소 계층까지 끌고 내려갈 이유가 없다.
    fn into_domain(self) -> Result<Schedule> {
        let id = ScheduleId::parse(&self.id)?;
        let profile = ProfileName::parse(&self.profile, Lang::Ko)?;
        let last_job_id = match self.last_job_id.as_deref() {
            Some(raw) => Some(JobId::parse(raw)?),
            None => None,
        };
        Ok(Schedule {
            id,
            profile: profile.as_str().to_string(),
            expr: self.expr,
            kind: self.kind,
            enabled: self.enabled,
            created_at: self.created_at,
            last_run_at: self.last_run_at,
            last_job_id,
        })
    }

    /// 도메인 값을 저장 표현으로.
    fn from_domain(schedule: &Schedule) -> Self {
        Self {
            id: schedule.id.to_string(),
            profile: schedule.profile.clone(),
            expr: schedule.expr.clone(),
            kind: schedule.kind,
            enabled: schedule.enabled,
            created_at: schedule.created_at,
            last_run_at: schedule.last_run_at,
            last_job_id: schedule.last_job_id.map(|id| id.to_string()),
        }
    }
}

// ---------------------------------------------------------------------------
// 로드 진단
// ---------------------------------------------------------------------------

/// 로드가 무엇을 만났는지 — 기동 로그·배너·화면 배너가 공유하는 하나의 사실.
///
/// 이 구조체가 따로 있는 이유: 손상을 알리는 세 경로(로그·배너·화면)가 각자 판단하면
/// 문구가 갈라지고, 그중 하나가 조용해지는 사고가 난다. 판단은 여기 한 번만 있고
/// ([`LoadReport::degraded`]), 세 경로는 같은 값을 읽는다.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoadReport {
    /// 스케줄 파일이 존재했는지. `false`면 신규 설치다(경고 대상이 아니다).
    pub file_present: bool,
    /// 성공적으로 접힌 스케줄 수.
    pub loaded: usize,
    /// 접지 못한 **항목** 수(표현식 손상·알 수 없는 종류·잘못된 프로파일명 등).
    pub unreadable_entries: usize,
    /// 파일 전체를 읽지 못한 사유(있으면). 0바이트·깨진 JSON·권한 문제·모르는 스키마 버전.
    pub fatal_error: Option<String>,
    /// 손상 본문을 [`SCHEDULES_CORRUPT_FILE_NAME`]으로 보존했는지.
    pub corrupt_copy_saved: bool,
    /// 직전본([`SCHEDULES_PREV_FILE_NAME`])에서 복구했는지.
    pub recovered_from_prev: bool,
}

impl LoadReport {
    /// 운영자가 **반드시 알아야 하는 상태**인지 — 스케줄이 의도대로 돌지 않을 수 있다.
    ///
    /// 신규 설치(파일 없음)는 여기 포함되지 않는다. 스케줄을 아직 만들지 않은 것은 정상이고,
    /// 그것까지 경고하면 진짜 신호가 묻힌다([`crate::web::reattach::ReattachReport::log`]와
    /// 같은 판단).
    pub fn degraded(&self) -> bool {
        self.fatal_error.is_some() || self.unreadable_entries > 0
    }

    /// 기동 로그로 남긴다. 정상이면 아무것도 찍지 않는다.
    ///
    /// 손상은 `error!`로 남긴다 — [`super::jobs`]의 손상이 `warn!`인 것과 다르다. 잡 이력의
    /// 손상은 화면 한 장을 잃는 일이고, 여기의 손상은 **백업이 멈추는** 일이다.
    pub fn log(&self, path: &Path) {
        if let Some(reason) = &self.fatal_error {
            tracing::error!(
                path = %path.display(),
                reason = %reason,
                recovered_from_prev = self.recovered_from_prev,
                loaded = self.loaded,
                corrupt_copy = self.corrupt_copy_saved,
                "스케줄 정의 파일을 읽을 수 없습니다 — 이 파일은 잡 이력과 달리 어디에서도 \
                 재구성할 수 없는 상태이고, 비어 있으면 예약된 백업이 조용히 멈춥니다. \
                 화면(/schedule)에서 스케줄을 확인·재등록하세요"
            );
            if self.corrupt_copy_saved {
                tracing::error!(
                    copy = %path.with_file_name(SCHEDULES_CORRUPT_FILE_NAME).display(),
                    "손상된 본문을 보존했습니다(원본도 그 자리에 그대로 있습니다) — \
                     화면에서 저장하면 온전한 내용으로 교체됩니다"
                );
            }
            if self.recovered_from_prev {
                tracing::warn!(
                    loaded = self.loaded,
                    "직전본에서 스케줄을 복구했습니다 — 마지막 변경 이후의 수정은 반영되지 \
                     않았을 수 있으니 화면에서 확인하세요"
                );
            }
            return;
        }
        if self.unreadable_entries > 0 {
            tracing::error!(
                path = %path.display(),
                unreadable_entries = self.unreadable_entries,
                loaded = self.loaded,
                "스케줄 항목 일부를 읽지 못했습니다 — 그 스케줄의 백업은 돌지 않습니다. \
                 화면(/schedule)에서 누락된 항목을 확인해 다시 등록하세요"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 저장소
// ---------------------------------------------------------------------------

/// 스케줄 저장소 — 파일 + 메모리 스냅샷.
///
/// **[`shared`]로만 얻는다**(모듈 헤더 "쓰기 경로는 인스턴스를 공유해야 한다").
#[derive(Debug)]
pub struct ScheduleStore {
    path: PathBuf,
    prev_path: PathBuf,
    /// 쓰기(복사 → 원자적 교체 → 스냅샷 갱신) 전체를 직렬화한다. [`super::jobs`]와 같은
    /// 이유로 `tokio::sync::Mutex`다(await 경계를 넘겨 들고 있어야 한다).
    write_lock: tokio::sync::Mutex<()>,
    /// 읽기 경로가 보는 스냅샷. 화면과 스케줄러 루프가 초당 여러 번 읽으므로 표준
    /// [`RwLock`]으로 둔다(await 없는 짧은 임계 구역이라 tokio 뮤텍스가 필요 없다).
    snapshot: RwLock<Vec<Schedule>>,
    /// 로드 결과. 화면 배너가 읽는다.
    report: RwLock<LoadReport>,
    /// 현재 디스크 본문을 신뢰할 수 있는지 — `false`면 다음 쓰기가 직전본 갱신을
    /// **건너뛴다**(모듈 헤더 "손상된 본문을 직전본에 밀어 넣지 않는다").
    main_trusted: RwLock<bool>,
}

impl ScheduleStore {
    /// 파일을 읽어 저장소를 만든다. **실패해도 인스턴스를 돌려준다**(fail-loud).
    ///
    /// 디렉터리를 만들지 않는다 — state 디렉터리 준비는 [`crate::web`]의 `ensure_state_dir`가
    /// 기동 시점에 이미 끝냈다.
    fn load(state_dir: &Path) -> Self {
        let path = state_dir.join(SCHEDULES_FILE_NAME);
        let prev_path = state_dir.join(SCHEDULES_PREV_FILE_NAME);
        let corrupt_path = state_dir.join(SCHEDULES_CORRUPT_FILE_NAME);

        let (schedules, report) = read_with_recovery(&path, &prev_path, &corrupt_path);
        let trusted = !report.degraded();
        Self {
            path,
            prev_path,
            write_lock: tokio::sync::Mutex::new(()),
            snapshot: RwLock::new(schedules),
            report: RwLock::new(report),
            main_trusted: RwLock::new(trusted),
        }
    }

    /// 스케줄 파일 경로(진단·로그용). **화면에 넣지 않는다** — 뷰 계층은 서버 내부 경로를
    /// 노출하지 않는다([`crate::web::view`] 헤더).
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 현재 스냅샷(id 오름차순 = 등록순).
    pub fn list(&self) -> Vec<Schedule> {
        read_lock(&self.snapshot).clone()
    }

    /// id로 하나를 찾는다.
    pub fn get(&self, id: ScheduleId) -> Option<Schedule> {
        read_lock(&self.snapshot)
            .iter()
            .find(|s| s.id == id)
            .cloned()
    }

    /// 로드 결과 — 화면 배너가 읽는다.
    pub fn report(&self) -> LoadReport {
        read_lock(&self.report).clone()
    }

    /// 스케줄을 추가한다.
    ///
    /// `receipt`를 **값으로** 받는 것이 이 시그니처의 요점이다. [`AuditReceipt`]는
    /// [`crate::web::audit::AuditLog::gate`]가 append에 성공했을 때만 만들어지고 그 모듈
    /// 밖에서는 생성할 수 없으므로, **감사 기록을 건너뛰고 스케줄을 바꾸는 코드는 컴파일되지
    /// 않는다**(t8 헤더의 핵심 불변식, t27의 `ConfigStore::apply`와 같은 형태).
    ///
    /// 스케줄 변경에 config 변경과 같은 강제를 거는 근거: 둘 다 "지금 무엇이 돌 것인가"를
    /// 바꾸는 영속 상태 변경이고, 스케줄 삭제는 파일을 하나도 지우지 않으면서 **백업을
    /// 멈춘다**. 파괴적이지 않다는 것이 기록하지 않아도 된다는 뜻은 아니다.
    pub async fn create(&self, draft: ScheduleDraft, receipt: AuditReceipt) -> Result<Schedule> {
        // receipt를 즉시 소비한다 — 값을 들고만 있으면 "기록 후 실행"이라는 계약이
        // 이 함수 안에서 흐려진다.
        let _ = receipt;
        let now = Utc::now();
        let created = Schedule {
            id: ScheduleId::generate(),
            profile: draft.profile.as_str().to_string(),
            expr: draft.expr,
            kind: draft.kind,
            enabled: draft.enabled,
            created_at: now,
            last_run_at: None,
            last_job_id: None,
        };

        let guard = self.write_lock.lock().await;
        let mut next = read_lock(&self.snapshot).clone();
        if next.len() >= MAX_SCHEDULES {
            return Err(XBackupError::Usage(format!(
                "스케줄이 이미 {MAX_SCHEDULES}개입니다 — 더 추가하려면 쓰지 않는 스케줄을 \
                 먼저 삭제하세요."
            )));
        }
        if let Some(existing) = next.iter().find(|s| same_trigger(s, &created)) {
            return Err(XBackupError::Usage(format!(
                "같은 프로파일·표현식·종류의 스케줄이 이미 있습니다(id {}) — 중복 등록은 \
                 같은 시각에 두 백업을 띄워 뒤쪽이 파일 락에 걸리게 만들므로 거부합니다.",
                existing.id.short()
            )));
        }
        next.push(created.clone());
        self.persist(&guard, next).await?;
        Ok(created)
    }

    /// 스케줄 하나를 수정한다(프로파일·표현식·종류·활성 여부).
    pub async fn update(
        &self,
        id: ScheduleId,
        draft: ScheduleDraft,
        receipt: AuditReceipt,
    ) -> Result<Schedule> {
        let _ = receipt;
        let guard = self.write_lock.lock().await;
        let mut next = read_lock(&self.snapshot).clone();
        let index = next
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| not_found(id))?;

        let mut updated = next[index].clone();
        updated.profile = draft.profile.as_str().to_string();
        updated.expr = draft.expr;
        updated.kind = draft.kind;
        updated.enabled = draft.enabled;

        if let Some(other) = next
            .iter()
            .find(|s| s.id != id && same_trigger(s, &updated))
        {
            return Err(XBackupError::Usage(format!(
                "수정 결과가 기존 스케줄(id {})과 같은 프로파일·표현식·종류가 됩니다 — \
                 중복 등록은 거부합니다.",
                other.id.short()
            )));
        }

        next[index] = updated.clone();
        self.persist(&guard, next).await?;
        Ok(updated)
    }

    /// 스케줄 하나를 삭제한다. 삭제된 값을 돌려준다(화면이 "무엇을 지웠는지" 말할 수 있게).
    pub async fn delete(&self, id: ScheduleId, receipt: AuditReceipt) -> Result<Schedule> {
        let _ = receipt;
        let guard = self.write_lock.lock().await;
        let mut next = read_lock(&self.snapshot).clone();
        let index = next
            .iter()
            .position(|s| s.id == id)
            .ok_or_else(|| not_found(id))?;
        let removed = next.remove(index);
        self.persist(&guard, next).await?;
        Ok(removed)
    }

    /// 발화 기록을 남긴다 — 스케줄러 루프 전용.
    ///
    /// ## 왜 여기에는 [`AuditReceipt`]가 없는가
    /// 이것은 운영자의 변경이 아니라 **스케줄러 자신의 부기**다. 그 발화로 떠오른 잡은
    /// [`crate::web::routes::backup::spawn_tracked_job`]이 `actor="scheduler"`로 감사 로그에
    /// 이미 남긴다(요청 시점 + 완료 시점 두 줄). 여기서 receipt를 또 요구하면 같은 사건이
    /// 감사 로그에 세 번 적히고, 그중 하나는 "누가 무엇을 바꿨나"가 아니라 "타이머가 돌았다"라서
    /// 감사 로그의 의미를 희석한다.
    ///
    /// **실패는 전파한다.** 이 기록이 빠지면 `last_run_at`이 갱신되지 않아 다음 기동에서
    /// "놓친 실행"이 과대 보고되고, 최악의 경우 같은 시각이 다시 발화 후보가 된다. 호출부
    /// (스케줄러 루프)가 그 실패를 로그로 크게 남긴다.
    pub async fn record_run(
        &self,
        id: ScheduleId,
        fired_at: DateTime<Utc>,
        job_id: Option<JobId>,
    ) -> Result<()> {
        let guard = self.write_lock.lock().await;
        let mut next = read_lock(&self.snapshot).clone();
        let Some(entry) = next.iter_mut().find(|s| s.id == id) else {
            // 발화 직후 운영자가 삭제했을 수 있다 — 되살리지 않는다.
            return Ok(());
        };
        entry.last_run_at = Some(fired_at);
        if job_id.is_some() {
            entry.last_job_id = job_id;
        }
        self.persist(&guard, next).await
    }

    /// 디스크에 쓰고 스냅샷을 갱신한다.
    ///
    /// ## 순서가 계약이다 — 디스크가 먼저다
    /// 스냅샷을 먼저 갱신하면 디스크 쓰기가 실패했을 때 **메모리만 참인 상태**가 된다.
    /// 화면은 스케줄이 등록됐다고 말하고, 재기동하면 사라진다 — "등록했는데 백업이 돌지
    /// 않았다"의 정확한 형태다. 그래서 디스크 쓰기가 성공한 뒤에만 스냅샷을 바꾼다.
    ///
    /// `_guard`는 쓰기 뮤텍스를 이 함수가 호출되는 동안 계속 들고 있다는 증거다(호출부가
    /// 잠그고 이 함수에 빌려준다) — 잠금 없이 부르는 경로를 시그니처로 막는다.
    async fn persist(
        &self,
        _guard: &tokio::sync::MutexGuard<'_, ()>,
        next: Vec<Schedule>,
    ) -> Result<()> {
        let stored: Vec<StoredSchedule> = next.iter().map(StoredSchedule::from_domain).collect();
        let bytes = serde_json::to_vec_pretty(&ScheduleFileOut {
            schema: SCHEDULES_SCHEMA,
            schedules: &stored,
        })
        .map_err(|e| XBackupError::Failure(format!("스케줄 정의 직렬화 실패: {e}")))?;

        let path = self.path.clone();
        let prev_path = self.prev_path.clone();
        // 손상된 본문을 직전본에 밀어 넣지 않는다(모듈 헤더).
        let keep_prev = *read_lock(&self.main_trusted);
        tokio::task::spawn_blocking(move || write_generation(&path, &prev_path, &bytes, keep_prev))
            .await
            .map_err(|e| {
                XBackupError::Failure(format!("스케줄 저장 태스크가 비정상 종료했습니다: {e}"))
            })??;

        *write_lock_of(&self.snapshot) = next;
        // 이제 디스크에 온전한 본문이 있다 — 이후 쓰기는 직전본을 정상적으로 갱신한다.
        *write_lock_of(&self.main_trusted) = true;
        Ok(())
    }
}

/// 같은 발화 조건인지 — 중복 등록 판정.
fn same_trigger(a: &Schedule, b: &Schedule) -> bool {
    a.profile == b.profile && a.kind == b.kind && a.expr == b.expr
}

/// "그런 스케줄이 없다" 오류. 404로 접힌다.
fn not_found(id: ScheduleId) -> XBackupError {
    XBackupError::Usage(format!(
        "스케줄 {}을 찾을 수 없습니다 — 다른 탭에서 이미 삭제되었을 수 있습니다.",
        id.short()
    ))
}

// ---------------------------------------------------------------------------
// 파일 읽기 — 손상 복구 포함
// ---------------------------------------------------------------------------

/// 본문 하나를 파싱한 결과.
enum ParsedFile {
    /// 접힌 스케줄 + 접지 못한 항목 수.
    Ok(Vec<Schedule>, usize),
    /// 파일 전체를 못 읽었다(사유 문구).
    Fatal(String),
}

/// 본문을 파싱한다 — 항목 하나가 깨져도 나머지는 살린다([`ScheduleFileIn`] doc).
fn parse_file(bytes: &[u8]) -> ParsedFile {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        // 0바이트는 크래시의 전형적 흔적이다. "스케줄 없음"의 정상 표현은
        // `{"schema":1,"schedules":[]}`이므로 빈 파일은 손상으로 본다 — 조용히 "없음"으로
        // 접으면 백업이 멈춘 사실이 사라진다.
        return ParsedFile::Fatal(
            "파일이 비어 있습니다(0바이트) — 쓰는 중 중단된 흔적입니다".to_string(),
        );
    }
    let parsed: ScheduleFileIn = match serde_json::from_slice(bytes) {
        Ok(value) => value,
        Err(e) => return ParsedFile::Fatal(format!("JSON을 해석할 수 없습니다: {e}")),
    };
    if parsed.schema > SCHEDULES_SCHEMA {
        // 상위 버전은 이 빌드가 모르는 필드에 의미가 있을 수 있다. 모르는 채로 읽어
        // "일부만 반영된 스케줄"을 돌리는 것보다, 읽지 못했다고 크게 알리는 것이 안전하다
        // (다운그레이드한 운영자가 그 사실을 알아야 한다).
        return ParsedFile::Fatal(format!(
            "스키마 버전 {}은 이 빌드({SCHEDULES_SCHEMA})가 모르는 상위 버전입니다 — \
             더 새 버전의 x-backup이 쓴 파일입니다",
            parsed.schema
        ));
    }

    let mut schedules = Vec::with_capacity(parsed.schedules.len());
    let mut unreadable = 0usize;
    for value in parsed.schedules {
        match serde_json::from_value::<StoredSchedule>(value)
            .map_err(|e| e.to_string())
            .and_then(|stored| stored.into_domain().map_err(|e| e.to_string()))
        {
            Ok(schedule) => schedules.push(schedule),
            Err(reason) => {
                unreadable += 1;
                tracing::error!(
                    reason = %reason,
                    "스케줄 항목 하나를 읽지 못했습니다 — 그 스케줄은 실행되지 않습니다"
                );
            }
        }
    }
    // id 오름차순 = 등록순(UUID v7). 파일 순서를 믿지 않는다(손으로 편집됐을 수 있다).
    schedules.sort_by_key(|s| s.id);
    ParsedFile::Ok(schedules, unreadable)
}

/// 파일을 읽고, 손상이면 직전본에서 복구를 시도한다(모듈 헤더 "손상 시 정책").
fn read_with_recovery(
    path: &Path,
    prev_path: &Path,
    corrupt_path: &Path,
) -> (Vec<Schedule>, LoadReport) {
    let mut report = LoadReport::default();

    let bytes = match std::fs::read(path) {
        Ok(bytes) => {
            report.file_present = true;
            bytes
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 신규 설치 — 경고 대상이 아니다.
            return (Vec::new(), report);
        }
        Err(e) => {
            // 읽을 수 없다(권한·EIO). 손상 사본을 만들려는 시도도 하지 않는다 — 읽을 수
            // 없는 파일은 복사할 수도 없다.
            report.file_present = true;
            report.fatal_error = Some(format!("파일을 읽을 수 없습니다: {e}"));
            return (Vec::new(), report);
        }
    };

    match parse_file(&bytes) {
        ParsedFile::Ok(schedules, unreadable) => {
            report.loaded = schedules.len();
            report.unreadable_entries = unreadable;
            (schedules, report)
        }
        ParsedFile::Fatal(reason) => {
            report.fatal_error = Some(reason);
            // 손상 본문 보존 — 복사만 한다(원본은 그 자리에 남긴다).
            report.corrupt_copy_saved = std::fs::write(corrupt_path, &bytes).is_ok();
            // 직전본에서 복구를 시도한다.
            if let Ok(prev_bytes) = std::fs::read(prev_path) {
                if let ParsedFile::Ok(schedules, unreadable) = parse_file(&prev_bytes) {
                    report.recovered_from_prev = true;
                    report.loaded = schedules.len();
                    report.unreadable_entries = unreadable;
                    return (schedules, report);
                }
            }
            (Vec::new(), report)
        }
    }
}

// ---------------------------------------------------------------------------
// 파일 쓰기 — 원자적 교체 + 직전본
// ---------------------------------------------------------------------------

/// 직전본을 갱신하고 새 내용을 원자적으로 교체한다. 동기 함수 —
/// [`ScheduleStore::persist`]의 `spawn_blocking` 안에서만 부른다.
fn write_generation(path: &Path, prev_path: &Path, bytes: &[u8], keep_prev: bool) -> Result<()> {
    // 직전본 복사가 **먼저**다. 순서를 뒤집으면(교체 후 복사) 직전본이 신본의 사본이 되어
    // 세대가 하나도 남지 않는다. 여기서 크래시하면 직전본 == 현재본이라 아무것도 잃지 않는다.
    if keep_prev && path.exists() {
        if let Err(e) = std::fs::copy(path, prev_path) {
            // 직전본은 보험이다 — 실패가 저장 자체를 막을 이유가 없다. 다만 조용히 넘기면
            // 보험이 없다는 사실을 아무도 모른다.
            tracing::warn!(
                prev = %prev_path.display(),
                error = %e,
                "스케줄 직전본을 남기지 못했습니다 — 저장은 계속합니다(복구 여유가 한 세대 줄어듭니다)"
            );
        }
    }
    write_atomic(path, bytes)
}

/// 임시 파일 → fsync → rename → 디렉터리 fsync.
///
/// 각 단계의 근거는 t27 `config_write.rs`의 `write_atomic` doc이 길게 적어 두었고 이 함수는
/// 같은 순서를 따른다. 코드를 재사용하지 않은 이유: 그 함수는 비공개이고 t28 소유 파일이다
/// (동시 작업 중이라 공용화는 후속 정리 대상이다).
///
/// 요약하면 (1) 임시 파일은 **같은 디렉터리**에 만든다 — `rename(2)`은 같은 파일시스템에서만
/// 원자적이다. (2) rename 전에 내용을 fsync한다 — 그러지 않으면 "이름은 새 파일을 가리키는데
/// 내용이 0바이트"가 정전 후에 남을 수 있고, 이 파일에서 그 상태는 곧 "백업이 멈춤"이다.
/// (3) rename. (4) 디렉터리 fsync는 **최선 노력**이다.
fn write_atomic(target: &Path, bytes: &[u8]) -> Result<()> {
    let dir = target.parent().ok_or_else(|| {
        XBackupError::Config(format!(
            "스케줄 파일 경로에 상위 디렉터리가 없습니다: {}",
            target.display()
        ))
    })?;
    let mut tmp = tempfile::Builder::new()
        .prefix(".x-backup-schedules-")
        .suffix(".tmp")
        .tempfile_in(dir)
        .map_err(XBackupError::Io)?;
    tmp.write_all(bytes).map_err(XBackupError::Io)?;
    tmp.as_file().sync_all().map_err(XBackupError::Io)?;
    // tempfile의 기본 권한(0600)을 그대로 쓴다. 목적지 권한을 물려받지 않는 것이
    // config와 다른 점이다 — 이 파일은 운영자가 공유할 이유가 없고, state 디렉터리
    // 자체가 0700이다.
    tmp.persist(target).map_err(|e| XBackupError::Io(e.error))?;

    #[cfg(unix)]
    if let Err(e) = std::fs::File::open(dir).and_then(|f| f.sync_all()) {
        tracing::warn!(
            dir = %dir.display(),
            error = %e,
            "state 디렉터리 fsync 실패 — 파일 내용은 이미 안전하다(최선 노력 단계)"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 잠금 헬퍼 — poison을 무시하고 계속한다
// ---------------------------------------------------------------------------

/// 읽기 잠금. poison은 무시한다 — 이 안에서 패닉이 나도 데이터 구조는 온전하고
/// (단순 대입뿐이다), 여기서 패닉을 전파하면 스케줄 화면이 영구히 500이 된다
/// (`routes::backup`의 허브 레지스트리와 같은 판단).
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// 쓰기 잠금(같은 판단).
fn write_lock_of<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

// ---------------------------------------------------------------------------
// 프로세스 전역 공유 인스턴스
// ---------------------------------------------------------------------------

/// state 디렉터리별 공유 인스턴스 레지스트리.
fn shared_registry() -> &'static std::sync::Mutex<HashMap<PathBuf, Arc<ScheduleStore>>> {
    static REGISTRY: OnceLock<std::sync::Mutex<HashMap<PathBuf, Arc<ScheduleStore>>>> =
        OnceLock::new();
    REGISTRY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// 이 state 디렉터리의 공유 저장소를 얻는다 — **없으면 이 호출이 파일을 로드한다.**
///
/// [`super::jobs::shared`]와 같은 형태이고 같은 이유다: 쓰기 직렬화(뮤텍스)와 메모리
/// 스냅샷이 인스턴스에 붙어 있어, 요청마다 새로 만들면 둘 다 무의미해진다.
/// `ServeConfig`에 필드를 얹는 것이 정공법이지만 `src/web/mod.rs`는 이 태스크가 `pub mod`
/// 한 줄 외에는 손대지 않는 파일이다.
///
/// 첫 호출이 작은 JSON 하나를 **동기적으로** 읽는다(`JobStore::open`이 `ensure_dir`를
/// 동기로 하는 것과 같은 수준의 비용이다). 기동 경로에서 먼저 부르므로
/// ([`crate::web::schedule::run_at_startup`]) 요청 처리 중에 이 읽기가 일어나는 일은 없다.
pub fn shared(state_dir: &Path) -> Arc<ScheduleStore> {
    let mut registry = shared_registry()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    Arc::clone(
        registry
            .entry(state_dir.to_path_buf())
            .or_insert_with(|| Arc::new(ScheduleStore::load(state_dir))),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::audit::AuditLog;

    /// 감사 receipt 하나를 만든다.
    ///
    /// 손으로 만들 수 없는 타입이므로([`AuditReceipt`] doc) 실제 감사 로그를 열어
    /// [`AuditLog::gate`]를 통과시킨다 — 테스트가 프로덕션과 같은 문을 지난다.
    async fn receipt(dir: &Path) -> AuditReceipt {
        AuditLog::open(dir)
            .expect("감사 로그 열기 실패")
            .gate("test", "schedule.create", "prod", &[])
            .await
            .expect("게이트 통과 실패")
    }

    fn draft(profile: &str, expr: &str, kind: ScheduleKind) -> ScheduleDraft {
        ScheduleDraft {
            profile: ProfileName::parse(profile, Lang::En).expect("프로파일명"),
            expr: CronExpr::parse(expr, Lang::En).expect("표현식"),
            kind,
            enabled: true,
        }
    }

    /// 임시 state 디렉터리 + 그 디렉터리에 직접 로드한 저장소(공유 레지스트리를 거치지
    /// 않는다 — 테스트 간 인스턴스 공유를 피한다).
    fn open_store(dir: &Path) -> ScheduleStore {
        ScheduleStore::load(dir)
    }

    /// 파일이 없으면 조용히 빈 목록이고 경고 대상이 아니다(신규 설치).
    #[test]
    fn missing_file_is_a_fresh_install_not_a_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        assert!(store.list().is_empty());
        let report = store.report();
        assert!(!report.file_present);
        assert!(
            !report.degraded(),
            "신규 설치가 손상으로 보고됐다: {report:?}"
        );
    }

    /// 생성 → 수정 → 삭제가 디스크와 스냅샷에 모두 반영된다.
    #[tokio::test]
    async fn crud_round_trips_through_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());

        let created = store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .expect("생성 실패");
        assert_eq!(store.list().len(), 1);

        // 새 인스턴스로 다시 읽어도 남아 있다 = 디스크에 실제로 썼다.
        let reloaded = open_store(tmp.path());
        assert_eq!(reloaded.list().len(), 1);
        assert_eq!(reloaded.list()[0].expr.as_str(), "0 3 * * *");
        assert!(reloaded.report().file_present);
        assert!(!reloaded.report().degraded());

        let updated = store
            .update(
                created.id,
                draft("prod", "*/30 * * * *", ScheduleKind::Incr),
                receipt(tmp.path()).await,
            )
            .await
            .expect("수정 실패");
        assert_eq!(updated.kind, ScheduleKind::Incr);
        assert_eq!(
            open_store(tmp.path()).list()[0].expr.as_str(),
            "*/30 * * * *"
        );

        let removed = store
            .delete(created.id, receipt(tmp.path()).await)
            .await
            .expect("삭제 실패");
        assert_eq!(removed.id, created.id);
        assert!(
            open_store(tmp.path()).list().is_empty(),
            "삭제가 디스크에 반영되지 않았다"
        );
    }

    /// 같은 프로파일·표현식·종류의 중복 등록은 거부된다 — 같은 시각에 두 백업을 띄우면
    /// 뒤쪽이 파일 락에 걸린다.
    #[tokio::test]
    async fn duplicate_trigger_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .unwrap();
        let err = store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .expect_err("중복은 거부되어야 함");
        assert_eq!(err.exit_code(), crate::error::exit_codes::USAGE);
        assert!(err.to_string().contains("이미 있습니다"), "{err}");

        // 종류가 다르면 허용된다(풀 + 증분 조합은 정상 운영 형태다).
        store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Incr),
                receipt(tmp.path()).await,
            )
            .await
            .expect("종류가 다른 스케줄은 허용되어야 함");
    }

    /// 상한을 넘기면 거부된다.
    #[tokio::test]
    async fn schedule_count_is_capped() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        for i in 0..MAX_SCHEDULES {
            store
                .create(
                    // 분·시를 함께 돌려 표현식이 겹치지 않게 한다(중복 등록은 거부된다).
                    draft(
                        "prod",
                        &format!("{} {} * * *", i % 60, i / 60),
                        ScheduleKind::Full,
                    ),
                    receipt(tmp.path()).await,
                )
                .await
                .unwrap_or_else(|e| panic!("{i}번째 생성 실패: {e}"));
        }
        let err = store
            .create(
                draft("other", "0 4 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .expect_err("상한을 넘기면 거부되어야 함");
        assert!(
            err.to_string().contains(&MAX_SCHEDULES.to_string()),
            "{err}"
        );
    }

    /// **깨진 JSON**에서 로드가 계속되고 크게 경고한다(fail-loud, 기동 거부 아님).
    #[test]
    fn broken_json_keeps_loading_and_reports_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SCHEDULES_FILE_NAME);
        std::fs::write(&path, b"{\"schema\":1,\"schedules\":[{\"id\"").unwrap();
        let store = open_store(tmp.path());
        assert!(store.list().is_empty());
        let report = store.report();
        assert!(report.degraded(), "손상이 보고되지 않았다");
        assert!(report.fatal_error.is_some());
        assert!(report.corrupt_copy_saved, "손상 본문을 보존하지 않았다");
        // **원본을 치우지 않는다** — 다음 기동에서도 같은 손상을 다시 감지해야 한다.
        assert!(
            path.exists(),
            "손상된 원본이 사라졌다(복구가 멱등하지 않다)"
        );
        assert!(tmp.path().join(SCHEDULES_CORRUPT_FILE_NAME).exists());
        report.log(&path);
    }

    /// **0바이트** 파일은 "스케줄 없음"이 아니라 손상이다.
    #[test]
    fn zero_byte_file_is_corruption_not_emptiness() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(SCHEDULES_FILE_NAME);
        std::fs::write(&path, b"").unwrap();
        let store = open_store(tmp.path());
        let report = store.report();
        assert!(report.degraded(), "0바이트가 정상으로 취급됐다");
        assert!(report.fatal_error.as_deref().unwrap().contains("0바이트"));
    }

    /// **잘린 줄**(끝이 잘린 JSON 배열)도 같은 경로로 잡힌다.
    #[test]
    fn truncated_file_is_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let good = format!(
            r#"{{"schema":{SCHEDULES_SCHEMA},"schedules":[{{"id":"{}","profile":"prod","expr":"0 3 * * *","kind":"full","enabled":true,"created_at":"2026-07-25T00:00:00Z"}}]}}"#,
            ScheduleId::generate()
        );
        let path = tmp.path().join(SCHEDULES_FILE_NAME);
        // 끝 20바이트를 잘라 낸다 — 쓰다가 죽은 파일의 모양.
        std::fs::write(&path, &good.as_bytes()[..good.len() - 20]).unwrap();
        assert!(open_store(tmp.path()).report().degraded());
    }

    /// 손상이면 **직전본에서 복구**한다.
    #[test]
    fn corruption_recovers_from_previous_generation() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ScheduleId::generate();
        let good = format!(
            r#"{{"schema":{SCHEDULES_SCHEMA},"schedules":[{{"id":"{id}","profile":"prod","expr":"0 3 * * *","kind":"full","enabled":true,"created_at":"2026-07-25T00:00:00Z"}}]}}"#
        );
        std::fs::write(tmp.path().join(SCHEDULES_PREV_FILE_NAME), &good).unwrap();
        std::fs::write(tmp.path().join(SCHEDULES_FILE_NAME), b"not json at all").unwrap();

        let store = open_store(tmp.path());
        let report = store.report();
        assert!(report.recovered_from_prev, "직전본 복구가 일어나지 않았다");
        assert_eq!(store.list().len(), 1, "복구된 스케줄이 없다");
        assert_eq!(store.list()[0].id, id);
        // 여전히 degraded다 — 운영자는 확인해야 한다.
        assert!(report.degraded());
    }

    /// 손상 상태에서의 첫 쓰기는 **직전본을 덮지 않는다** — 마지막 온전한 사본을 지키는 것이
    /// 이 규칙의 목적이다.
    #[tokio::test]
    async fn write_after_corruption_preserves_the_last_good_previous() {
        let tmp = tempfile::tempdir().unwrap();
        let id = ScheduleId::generate();
        let good = format!(
            r#"{{"schema":{SCHEDULES_SCHEMA},"schedules":[{{"id":"{id}","profile":"prod","expr":"0 3 * * *","kind":"full","enabled":true,"created_at":"2026-07-25T00:00:00Z"}}]}}"#
        );
        let prev_path = tmp.path().join(SCHEDULES_PREV_FILE_NAME);
        std::fs::write(&prev_path, &good).unwrap();
        std::fs::write(tmp.path().join(SCHEDULES_FILE_NAME), b"corrupt").unwrap();

        let store = open_store(tmp.path());
        store
            .create(
                draft("other", "0 4 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .expect("손상 상태에서도 저장은 가능해야 한다");

        let prev_now = std::fs::read_to_string(&prev_path).unwrap();
        assert!(
            !prev_now.contains("corrupt"),
            "손상된 본문이 직전본을 덮었다: {prev_now}"
        );
        assert!(prev_now.contains(&id.to_string()), "직전본이 바뀌었다");

        // 두 번째 쓰기부터는 정상적으로 세대가 돈다(디스크에 온전한 본문이 있으므로).
        store
            .create(
                draft("third", "0 5 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .unwrap();
        let prev_now = std::fs::read_to_string(&prev_path).unwrap();
        assert!(prev_now.contains("other"), "직전본 갱신이 재개되지 않았다");
    }

    /// **항목 하나가 깨져도 나머지는 살아남는다** — 그리고 그 개수를 센다.
    #[test]
    fn one_bad_entry_does_not_kill_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let good_id = ScheduleId::generate();
        let body = format!(
            r#"{{"schema":{SCHEDULES_SCHEMA},"schedules":[
              {{"id":"{good_id}","profile":"prod","expr":"0 3 * * *","kind":"full","enabled":true,"created_at":"2026-07-25T00:00:00Z"}},
              {{"id":"{}","profile":"broken","expr":"60 * * * *","kind":"full","enabled":true,"created_at":"2026-07-25T00:00:00Z"}},
              {{"id":"{}","profile":"prod","expr":"0 4 * * *","kind":"weekly","enabled":true,"created_at":"2026-07-25T00:00:00Z"}}
            ]}}"#,
            ScheduleId::generate(),
            ScheduleId::generate()
        );
        std::fs::write(tmp.path().join(SCHEDULES_FILE_NAME), body).unwrap();
        let store = open_store(tmp.path());
        assert_eq!(store.list().len(), 1, "온전한 항목이 함께 버려졌다");
        assert_eq!(store.list()[0].id, good_id);
        let report = store.report();
        assert_eq!(report.unreadable_entries, 2, "깨진 항목 수가 어긋난다");
        assert!(report.degraded());
        // 파일 전체가 실패한 것은 아니다 — 이 구분이 화면 문구를 가른다.
        assert!(report.fatal_error.is_none());
    }

    /// 모르는 상위 스키마 버전은 읽지 않고 크게 알린다.
    #[test]
    fn unknown_future_schema_is_refused_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(SCHEDULES_FILE_NAME),
            format!(r#"{{"schema":{},"schedules":[]}}"#, SCHEDULES_SCHEMA + 1),
        )
        .unwrap();
        let report = open_store(tmp.path()).report();
        assert!(report.fatal_error.as_deref().unwrap().contains("상위 버전"));
    }

    /// 원자적 저장 — 임시 파일이 남지 않고, 교체 중 크래시는 원본 또는 신본만 남긴다.
    ///
    /// 크래시를 직접 주입할 수는 없으므로 그 성질을 두 조각으로 나눠 확인한다:
    /// (1) `persist`를 부르지 않고 임시 파일만 만들어 버리면 목적지가 그대로다(= rename
    /// 전에는 목적지가 절대 변하지 않는다), (2) 정상 경로 후 디렉터리에 `.tmp`가 남지 않는다.
    #[tokio::test]
    async fn atomic_write_leaves_only_whole_generations() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .unwrap();
        let path = tmp.path().join(SCHEDULES_FILE_NAME);
        let before = std::fs::read(&path).unwrap();

        // rename 전에 죽은 경우를 흉내낸다 — 임시 파일을 만들고 persist 없이 버린다.
        {
            let mut tmpfile = tempfile::Builder::new()
                .prefix(".x-backup-schedules-")
                .suffix(".tmp")
                .tempfile_in(tmp.path())
                .unwrap();
            tmpfile.write_all(b"half written").unwrap();
            tmpfile.as_file().sync_all().unwrap();
        }
        assert_eq!(
            std::fs::read(&path).unwrap(),
            before,
            "rename 전에 목적지가 변했다"
        );

        // 정상 경로 후 임시 파일이 남지 않는다.
        store
            .create(
                draft("other", "0 4 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "임시 파일이 남았다: {leftovers:?}");
        // 그리고 최종 파일은 항상 온전한 JSON이다.
        assert!(matches!(
            parse_file(&std::fs::read(&path).unwrap()),
            ParsedFile::Ok(_, 0)
        ));
    }

    /// 발화 기록이 남고, 삭제된 스케줄을 되살리지 않는다.
    #[tokio::test]
    async fn record_run_persists_and_never_resurrects() {
        let tmp = tempfile::tempdir().unwrap();
        let store = open_store(tmp.path());
        let created = store
            .create(
                draft("prod", "0 3 * * *", ScheduleKind::Full),
                receipt(tmp.path()).await,
            )
            .await
            .unwrap();
        let job = JobId::generate();
        let fired = Utc::now();
        store
            .record_run(created.id, fired, Some(job))
            .await
            .unwrap();

        let reloaded = open_store(tmp.path());
        let entry = &reloaded.list()[0];
        assert_eq!(entry.last_job_id, Some(job));
        assert!(entry.last_run_at.is_some());

        // 삭제 후의 발화 기록은 조용히 무시된다(되살리면 지운 스케줄이 계속 돈다).
        store
            .delete(created.id, receipt(tmp.path()).await)
            .await
            .unwrap();
        store
            .record_run(created.id, Utc::now(), None)
            .await
            .unwrap();
        assert!(
            open_store(tmp.path()).list().is_empty(),
            "삭제된 스케줄이 되살아났다"
        );
    }

    /// 비활성 스케줄은 다음 실행 시각이 없다.
    #[test]
    fn disabled_schedule_has_no_next_run() {
        let mut schedule = Schedule {
            id: ScheduleId::generate(),
            profile: "prod".to_string(),
            expr: CronExpr::parse("0 3 * * *", Lang::En).unwrap(),
            kind: ScheduleKind::Full,
            enabled: true,
            created_at: Utc::now(),
            last_run_at: None,
            last_job_id: None,
        };
        assert!(schedule.next_run(Utc::now()).is_some());
        schedule.enabled = false;
        assert!(schedule.next_run(Utc::now()).is_none());
    }

    /// 놓친 실행 개수는 정확하고 상한에서 멈춘다.
    #[test]
    fn missed_runs_are_counted_and_capped() {
        let base = DateTime::parse_from_rfc3339("2026-07-20T00:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let daily = Schedule {
            id: ScheduleId::generate(),
            profile: "prod".to_string(),
            expr: CronExpr::parse("0 3 * * *", Lang::En).unwrap(),
            kind: ScheduleKind::Full,
            enabled: true,
            created_at: base,
            last_run_at: None,
            last_job_id: None,
        };
        // 7/20 00:00 ~ 7/25 12:00 사이의 03:00은 20·21·22·23·24·25 = 6번.
        let now = DateTime::parse_from_rfc3339("2026-07-25T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        assert_eq!(daily.missed_since(base, now), 6);
        // 기준선이 곧 지금이면 0이다.
        assert_eq!(daily.missed_since(now, now), 0);

        // 매분 스케줄이 아주 오래 멈춰 있으면 상한에서 멈춘다.
        let minutely = Schedule {
            expr: CronExpr::parse("* * * * *", Lang::En).unwrap(),
            ..daily.clone()
        };
        let far = base + chrono::TimeDelta::days(30);
        assert_eq!(minutely.missed_since(base, far), MISSED_SCAN_MAX);
    }

    /// 공유 인스턴스는 같은 state 디렉터리에 대해 항상 같은 포인터다.
    #[test]
    fn shared_returns_one_instance_per_state_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let a = shared(tmp.path());
        let b = shared(tmp.path());
        assert!(
            Arc::ptr_eq(&a, &b),
            "인스턴스가 갈라지면 쓰기 직렬화가 사라진다"
        );
    }
}
