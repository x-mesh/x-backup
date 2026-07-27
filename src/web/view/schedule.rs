//! `GET /schedule` 계열 화면 마크업 — 목록·생성/수정 폼·삭제 확인.
//!
//! ## 이 화면의 가장 중요한 일: **기준 시각이 UTC라고 말하는 것**
//! cron 표현식은 UTC로 해석된다([`crate::web::schedule::cron`] 헤더의 DST 근거). 운영자는
//! 거의 항상 "매일 새벽 3시"를 **로컬 시각으로** 생각하므로, 그 어긋남을 화면이 갚지 않으면
//! 백업이 의도와 9시간(KST) 다른 시각에 돈다. 그래서 세 곳에서 반복해 말한다:
//!
//! 1. 화면 상단 고정 안내 — "표현식은 UTC로 해석된다".
//! 2. 표현식 입력 라벨이 `Cron (UTC)`다(그냥 `Cron`이 아니다).
//! 3. 다음 실행 시각을 **UTC 한 줄 + 서버 로컬 한 줄**로 함께 보여준다.
//!
//! 3번이 특히 중요하다 — 운영자는 대개 로컬 시각으로 검산한다. 두 표기를 나란히 두면
//! "03:00 UTC = 정오 KST"라는 사실을 표현식을 저장하기 전에 눈으로 확인한다.
//!
//! ## 손상 배너는 이 화면의 첫 줄이다
//! 스케줄 파일이 온전히 읽히지 않았으면([`LoadReport::degraded`]) 그 사실이 목록보다 먼저
//! 나온다. 그 상황에서 **빈 목록은 "스케줄이 없다"가 아니라 "예약된 백업이 멈췄다"**이고,
//! 두 상태가 화면에서 같아 보이면 이 기능의 존재 이유가 무너진다.
//!
//! ## 놓친 실행 표시는 **한 번이라도 돈 스케줄에만** 붙인다
//! 아직 한 번도 돌지 않은 스케줄에 "N번 놓쳤다"를 붙이면 오해를 만든다. 스케줄러는 등록
//! 이전·기동 이전의 시각을 **의도적으로** 후보로 삼지 않으므로([`crate::web::schedule`]
//! 헤더의 놓친 실행 정책), 방금 등록한 매분 스케줄은 정의상 "지나간 시각"을 갖는다 — 그것은
//! 운영자가 조치할 gap이 아니라 정책의 산물이다. 반면 **한 번 돈 뒤에 생긴 gap**은 실제
//! 누락이므로 표시한다.
//!
//! ## 모양은 담지 않는다
//! [`components`] 헤더의 계약을 그대로 따른다 — 상태는 `data-level` 토큰으로만, 색·인라인
//! 스타일 리터럴은 마크업에 두지 않는다. 아래 `markup_carries_no_presentation` 테스트가
//! 그것을 고정한다.

use chrono::{DateTime, Local, Utc};
use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::routes::schedule as route;
use crate::web::schedule::CrontabScan;
use crate::web::state::schedules::{LoadReport, Schedule, ScheduleKind};
use crate::web::view::components::{self, Level};

/// 시각 표기 형식(UTC·로컬 공통). 초를 뺀 이유: 이 화면의 모든 시각은 분 단위 스케줄에서
/// 나오므로 초는 항상 `00`이고, 자리만 차지한다.
const TIME_FORMAT: &str = "%Y-%m-%d %H:%M";

/// 로컬 표기에 붙이는 형식 — 오프셋 약어(`KST`·`EST`)까지 찍어 어느 지역인지 드러낸다.
const LOCAL_TIME_FORMAT: &str = "%Y-%m-%d %H:%M %Z";

/// UTC 시각 한 줄.
fn utc_text(at: DateTime<Utc>) -> String {
    format!("{} UTC", at.format(TIME_FORMAT))
}

/// 서버 로컬 시각 한 줄 — 운영자가 검산하는 표기다(모듈 헤더 3번).
fn local_text(at: DateTime<Utc>) -> String {
    at.with_timezone(&Local)
        .format(LOCAL_TIME_FORMAT)
        .to_string()
}

/// `GET /schedule` 목록 본문.
///
/// `notice`는 직전 제출이 실패했을 때 위에 얹는 알림이다.
pub fn list_body(
    lang: Lang,
    schedules: &[Schedule],
    report: &LoadReport,
    crontab: &CrontabScan,
    now: DateTime<Utc>,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::SCHEDULE_TITLE, Some(lang.sel(
            "Built-in scheduler for periodic backups. Cron expressions are read in UTC.",
            "주기 백업을 도는 내장 스케줄러입니다. cron 표현식은 UTC로 해석됩니다.",
        ))))
        @if let Some(notice) = notice { (notice) }
        @if report.degraded() { (degraded_banner(lang, report)) }
        @if crontab.conflicts() { (crontab_notice(lang, crontab)) }
        (utc_basis_notice(lang))
        (components::panel(
            html! {
                h3 class="panel__title" { "Schedules" }
                span class="tag mono" { (schedules.len()) }
            },
            html! {
                @if schedules.is_empty() {
                    p { (lang.sel(
                        "No schedules yet. Periodic backups only run while this console is up — \
                         if you still have x-backup entries in crontab, move them here one at a time.",
                        "아직 스케줄이 없습니다. 주기 백업은 이 콘솔이 떠 있는 동안만 돕니다 — \
                         crontab에 x-backup 항목이 남아 있다면 하나씩 여기로 옮기세요.",
                    )) }
                } @else {
                    (schedule_table(lang, schedules, now))
                }
                div class="actions" {
                    a href=(route::NEW_PATH) { (lang.sel("New schedule", "스케줄 추가")) }
                }
            },
        ))
    }
}

/// 스케줄 목록 표.
fn schedule_table(lang: Lang, schedules: &[Schedule], now: DateTime<Utc>) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "State" }
                        th scope="col" { "Profile" }
                        th scope="col" { "Cron (UTC)" }
                        th scope="col" { "Type" }
                        th scope="col" { "Next run" }
                        th scope="col" { "Last run" }
                        th scope="col" { "Actions" }
                    }
                }
                tbody {
                    @for schedule in schedules {
                        (schedule_row(lang, schedule, now))
                    }
                }
            }
        }
    }
}

/// 표의 한 줄.
fn schedule_row(lang: Lang, schedule: &Schedule, now: DateTime<Utc>) -> Markup {
    let next = schedule.next_run(now);
    html! {
        tr {
            td { (state_badge(lang, schedule, next)) }
            td class="mono" { (schedule.profile) }
            td class="mono" { (schedule.expr.as_str()) }
            td class="mono" { (schedule.kind.token()) }
            td {
                @match next {
                    Some(at) => {
                        div class="mono" { (utc_text(at)) }
                        div class="muted" { (local_text(at)) }
                    }
                    None if !schedule.enabled => {
                        span class="muted" { (lang.sel("paused", "일시 중지")) }
                    }
                    None => {
                        span class="muted" { (lang.sel(
                            "never — this expression has no future match",
                            "없음 — 이 표현식은 앞으로 맞는 시각이 없습니다",
                        )) }
                    }
                }
            }
            td { (last_run_cell(lang, schedule, now)) }
            td {
                a href=(route::edit_href(schedule.id)) { (lang.sel("Edit", "수정")) }
                " "
                a href=(route::delete_confirm_href(schedule.id)) { (lang.sel("Delete", "삭제")) }
            }
        }
    }
}

/// 상태 배지 — 활성/일시 중지/발화 불가.
///
/// "활성인데 다음 실행이 없다"는 조용한 고장이다(예: `0 0 30 2 *`). 그 경우를 활성과 같은
/// 배지로 보여주면 운영자는 백업이 돌 것이라고 믿는다 — 그래서 별도 레벨로 가른다.
fn state_badge(lang: Lang, schedule: &Schedule, next: Option<DateTime<Utc>>) -> Markup {
    if !schedule.enabled {
        return html! {
            span class="badge" data-level=(Level::Warn.token()) { "PAUSED" }
        };
    }
    if next.is_none() {
        return html! {
            span class="badge" data-level=(Level::Fail.token()) title=(lang.sel(
                "Enabled but this expression never matches — no backup will run.",
                "활성이지만 이 표현식은 앞으로 맞는 시각이 없습니다 — 백업이 돌지 않습니다.",
            )) { "NEVER" }
        };
    }
    html! {
        span class="badge" data-level=(Level::Ok.token()) { "ACTIVE" }
    }
}

/// 마지막 실행 칸 — 잡 링크와 (있으면) 놓친 실행 수.
fn last_run_cell(lang: Lang, schedule: &Schedule, now: DateTime<Utc>) -> Markup {
    let Some(last) = schedule.last_run_at else {
        return html! {
            span class="muted" { (lang.sel("never run yet", "아직 실행되지 않음")) }
        };
    };
    // 한 번이라도 돈 뒤에 생긴 gap만 표시한다(모듈 헤더).
    let missed = if schedule.enabled {
        schedule.missed_since(last, now)
    } else {
        0
    };
    html! {
        div class="mono" { (utc_text(last)) }
        @if let Some(job) = schedule.last_job_id {
            div {
                a href=(crate::web::routes::jobs::job_detail_href(&job)) class="mono" {
                    (job.short())
                }
            }
        }
        @if missed > 0 {
            div {
                span class="badge" data-level=(Level::Warn.token()) { "MISSED" }
                " "
                span class="mono" { (missed_text(missed)) }
                p class="field__hint" { (lang.sel(
                    "Runs that came due while the console was down are not caught up.",
                    "콘솔이 꺼져 있는 동안 지나간 실행은 따라잡지 않습니다.",
                )) }
            }
        }
    }
}

/// 놓친 실행 수 표기 — 상한에 걸리면 `1000+`로 정직하게 말한다.
fn missed_text(missed: usize) -> String {
    if missed >= crate::web::state::schedules::MISSED_SCAN_MAX {
        format!("{missed}+")
    } else {
        missed.to_string()
    }
}

/// 손상 배너 — 이 화면의 첫 줄(모듈 헤더).
///
/// **경로를 싣지 않는다** — 뷰 계층은 서버 내부 경로를 노출하지 않는다
/// ([`crate::web::view`] 헤더). 파일 이름까지만 말하고 전체 경로는 기동 로그에 있다.
pub fn degraded_banner(lang: Lang, report: &LoadReport) -> Markup {
    let (level, headline) = if report.fatal_error.is_some() {
        (
            Level::Error,
            lang.sel(
                "The schedule definition file could not be read",
                "스케줄 정의 파일을 읽지 못했습니다",
            ),
        )
    } else {
        (
            Level::Fail,
            lang.sel(
                "Some schedule entries could not be read",
                "일부 스케줄 항목을 읽지 못했습니다",
            ),
        )
    };
    components::notice(
        level,
        headline,
        html! {
            p { (lang.sel(
                "Unlike job history, schedules cannot be rebuilt from anything else — an empty \
                 list here means periodic backups have silently stopped. Re-create the missing \
                 schedules below.",
                "스케줄은 잡 이력과 달리 어디에서도 재구성할 수 없습니다 — 목록이 비어 있다는 \
                 것은 예약된 백업이 조용히 멈췄다는 뜻입니다. 아래에서 누락된 스케줄을 다시 \
                 등록하세요.",
            )) }
            (components::meta_list(&[
                ("loaded", report.loaded.to_string()),
                ("unreadable entries", report.unreadable_entries.to_string()),
                (
                    "recovered from previous",
                    if report.recovered_from_prev { "yes" } else { "no" }.to_string(),
                ),
                (
                    "corrupt copy kept",
                    if report.corrupt_copy_saved {
                        crate::web::state::schedules::SCHEDULES_CORRUPT_FILE_NAME.to_string()
                    } else {
                        "no".to_string()
                    },
                ),
            ]))
            @if report.recovered_from_prev {
                p { (lang.sel(
                    "The list below was recovered from the previous generation of the file, so \
                     changes made just before the failure may be missing.",
                    "아래 목록은 파일의 직전본에서 복구한 것이므로, 손상 직전의 변경은 빠져 \
                     있을 수 있습니다.",
                )) }
            }
        },
    )
}

/// 외부 crontab 충돌 경고(t30).
pub fn crontab_notice(lang: Lang, crontab: &CrontabScan) -> Markup {
    components::notice(
        Level::Warn,
        lang.sel(
            "crontab still has x-backup entries",
            "crontab에 x-backup 항목이 남아 있습니다",
        ),
        html! {
            p { (lang.sel(
                "The same backup may be started twice — once by cron and once by this console. \
                 The file lock keeps your data safe (the later one exits 5), but the history will \
                 fill with warnings. Remove the crontab entry for anything you schedule here.",
                "같은 백업이 두 번 시작될 수 있습니다 — cron이 한 번, 이 콘솔이 한 번. 파일 \
                 락이 데이터를 지켜 주지만(뒤에 시작한 쪽이 exit 5) 이력에 경고가 쌓입니다. \
                 여기에 등록한 스케줄은 crontab에서 제거하세요.",
            )) }
            ul class="logdump" {
                @for entry in &crontab.entries {
                    li class="mono" { (entry) }
                }
            }
        },
    )
}

/// UTC 기준 고정 안내 — 화면 상단에 항상 있다(모듈 헤더 1번).
fn utc_basis_notice(lang: Lang) -> Markup {
    components::notice(
        Level::Ok,
        lang.sel("Times are UTC", "시각 기준은 UTC입니다"),
        html! {
            p { (lang.sel(
                "Cron fields are read in UTC, never in the server's local time zone. That is what \
                 makes daylight-saving transitions safe — a local-time scheduler would skip a run \
                 in spring and fire twice in autumn. Each next-run time below is shown in UTC and \
                 in server local time so you can check it before saving.",
                "cron 필드는 서버 로컬 시각이 아니라 UTC로 해석됩니다. 그래야 일광 절약 시간 \
                 전환이 안전합니다 — 로컬 기준 스케줄러는 봄 전환에서 한 번을 건너뛰고 가을 \
                 전환에서 두 번 실행합니다. 아래의 다음 실행 시각은 UTC와 서버 로컬 시각을 \
                 함께 보여 주므로 저장 전에 확인할 수 있습니다.",
            )) }
        },
    )
}

// ---------------------------------------------------------------------------
// 생성·수정 폼
// ---------------------------------------------------------------------------

/// 폼이 생성인지 수정인지.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormMode {
    /// 새 스케줄.
    New,
    /// 기존 스케줄 수정.
    Edit,
}

/// 폼에 미리 채울 값 — 검증 실패 후 되돌려 그릴 때 사용자가 적은 값이 유지된다.
///
/// 폼을 다시 그릴 때 입력을 비우면 운영자가 표현식을 처음부터 다시 적어야 하고, 그 마찰이
/// "일단 되는 표현식"을 적게 만든다. 그래서 실패한 값도 그대로 되돌려 준다 — maud가 자동
/// 이스케이프하므로 남의 문자열을 되돌리는 것 자체는 안전하다.
#[derive(Debug, Clone, Default)]
pub struct FormValues {
    /// 스케줄 id(수정일 때만).
    pub id: Option<String>,
    /// 프로파일명.
    pub profile: String,
    /// cron 표현식.
    pub expr: String,
    /// 백업 종류.
    pub kind: Option<ScheduleKind>,
    /// 활성 여부.
    pub enabled: bool,
}

impl FormValues {
    /// 기존 스케줄에서 폼 값을 만든다.
    pub fn from_schedule(schedule: &Schedule) -> Self {
        Self {
            id: Some(schedule.id.to_string()),
            profile: schedule.profile.clone(),
            expr: schedule.expr.as_str().to_string(),
            kind: Some(schedule.kind),
            enabled: schedule.enabled,
        }
    }
}

/// 생성·수정 폼 본문.
///
/// `profiles`는 config에서 읽은 프로파일 이름 목록이다. 비어 있으면 자유 입력으로 떨어진다 —
/// config를 읽을 수 없는 상황(경로 미지정)에서도 스케줄을 만들 수 있어야 하고, 프로파일
/// 존재 여부는 결국 자식이 exit 2로 판정한다(웹이 CLI 인자 표면을 이중 구현하지 않는다는
/// [`crate::web::job::spec`]의 원칙과 같다).
pub fn form_body(
    lang: Lang,
    mode: FormMode,
    values: &FormValues,
    profiles: &[String],
    preview: Option<DateTime<Utc>>,
    notice: Option<Markup>,
) -> Markup {
    let title = match mode {
        FormMode::New => lang.sel("New schedule", "스케줄 추가"),
        FormMode::Edit => lang.sel("Edit schedule", "스케줄 수정"),
    };
    html! {
        (components::page_head(route::SCHEDULE_TITLE, Some(title)))
        @if let Some(notice) = notice { (notice) }
        (utc_basis_notice(lang))
        (components::panel(
            html! { h3 class="panel__title" { (title) } },
            html! {
                form method="post" action=(route::SAVE_PATH) class="cfg-form" {
                    @if let Some(id) = &values.id {
                        input type="hidden" name=(route::FIELD_ID) value=(id);
                    }
                    div class="field" {
                        label class="field__label mono" for="s-profile" { "profile" }
                        @if profiles.is_empty() {
                            input id="s-profile" type="text" name=(route::FIELD_PROFILE)
                                value=(values.profile) required autocomplete="off";
                            p class="field__hint" { (lang.sel(
                                "No profiles were found in the config file — type the name.",
                                "config 파일에서 프로파일을 찾지 못했습니다 — 이름을 직접 적으세요.",
                            )) }
                        } @else {
                            select id="s-profile" name=(route::FIELD_PROFILE) required {
                                @for profile in profiles {
                                    @if *profile == values.profile {
                                        option value=(profile) selected { (profile) }
                                    } @else {
                                        option value=(profile) { (profile) }
                                    }
                                }
                            }
                        }
                    }
                    div class="field" {
                        label class="field__label mono" for="s-expr" { "Cron (UTC)" }
                        input id="s-expr" type="text" name=(route::FIELD_EXPR)
                            value=(values.expr) required autocomplete="off"
                            placeholder="0 3 * * *";
                        p class="field__hint" { (lang.sel(
                            "Five fields: minute hour day-of-month month day-of-week. \
                             `@hourly` and `@daily` are the only aliases. Read in UTC.",
                            "5필드: 분 시 일 월 요일. 별칭은 `@hourly`·`@daily` 둘뿐입니다. \
                             UTC로 해석됩니다.",
                        )) }
                        p class="field__hint mono" { "0 3 * * *  ·  */15 * * * *  ·  0 9-17 * * 1-5  ·  @daily" }
                    }
                    div class="field" {
                        label class="field__label mono" for="s-kind" { "type" }
                        select id="s-kind" name=(route::FIELD_KIND) {
                            @for kind in [ScheduleKind::Full, ScheduleKind::Incr] {
                                @if values.kind == Some(kind) {
                                    option value=(kind.token()) selected { (kind.token()) }
                                } @else {
                                    option value=(kind.token()) { (kind.token()) }
                                }
                            }
                        }
                    }
                    div class="field" {
                        label {
                            @if values.enabled {
                                input type="checkbox" name=(route::FIELD_ENABLED) value="1" checked;
                            } @else {
                                input type="checkbox" name=(route::FIELD_ENABLED) value="1";
                            }
                            " enabled"
                        }
                        p class="field__hint" { (lang.sel(
                            "Uncheck to pause without deleting. A paused schedule never fires.",
                            "삭제하지 않고 잠시 멈추려면 해제하세요. 일시 중지된 스케줄은 실행되지 않습니다.",
                        )) }
                    }
                    @if let Some(at) = preview {
                        div class="field" {
                            span class="field__label mono" { "next run" }
                            div class="mono" { (utc_text(at)) }
                            div class="muted" { (local_text(at)) }
                        }
                    }
                    div class="actions" {
                        button type="submit" { (lang.sel("Save schedule", "스케줄 저장")) }
                        " "
                        a href=(route::SCHEDULE_PATH) { (lang.sel("Cancel", "취소")) }
                    }
                }
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// 삭제 확인
// ---------------------------------------------------------------------------

/// 삭제 확인 화면.
///
/// ## 왜 확인 단계가 있는가 — 삭제는 파괴적이 아닌데도
/// 이 삭제는 파일을 하나도 지우지 않는다. 그런데도 확인을 요구하는 이유는 **결과가 조용하기
/// 때문**이다: 잘못 지워도 그 순간 아무 일도 일어나지 않고, 며칠 뒤 "백업이 없다"로 드러난다.
/// 되돌릴 수 없는 작업이 위험한 것과 **알아차릴 수 없는 작업이 위험한 것**은 다른 축이고, 이
/// 삭제는 후자다.
///
/// 확인 방식은 [`crate::web::routes::config`]의 프로파일 삭제와 같다 — 프로파일 이름을 그대로
/// 타이핑한다. 같은 콘솔에서 확인 방식이 화면마다 다르면 운영자가 "이번엔 뭘 적어야 하지"를
/// 매번 생각해야 하고, 그 마찰이 실제로는 대충 넘기는 습관을 만든다.
///
/// **t21의 파괴적 작업 가드로 승격 대상이다.** t21이 `src/web/guard.rs`에 공통 가드를 만들고
/// 있으므로(동시 작업), 여기 구현은 그 가드가 들어오면 그것으로 교체해야 한다 — 확인 문구·
/// 실패 메시지·감사 연동이 한 곳에 모이는 것이 옳다.
pub fn delete_confirm_body(
    lang: Lang,
    schedule: &Schedule,
    now: DateTime<Utc>,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::SCHEDULE_TITLE, Some(lang.sel(
            "Delete a schedule",
            "스케줄 삭제",
        ))))
        @if let Some(notice) = notice { (notice) }
        (components::notice(
            Level::Warn,
            lang.sel("This stops future backups silently", "앞으로의 백업이 조용히 멈춥니다"),
            html! {
                p { (lang.sel(
                    "Deleting a schedule removes no files and breaks nothing right now — which is \
                     exactly why it is easy to miss. The next run simply never happens. If you \
                     only want to pause it, edit the schedule and uncheck `enabled` instead.",
                    "스케줄을 삭제해도 파일이 지워지지 않고 지금 당장 아무것도 깨지지 않습니다 — \
                     그래서 알아차리기 어렵습니다. 다음 실행이 그냥 일어나지 않을 뿐입니다. \
                     잠시 멈추려는 것이라면 수정 화면에서 `enabled`를 해제하세요.",
                )) }
                (components::meta_list(&[
                    ("profile", schedule.profile.clone()),
                    ("cron (UTC)", schedule.expr.as_str().to_string()),
                    ("type", schedule.kind.token().to_string()),
                    (
                        "next run",
                        match schedule.next_run(now) {
                            Some(at) => format!("{} / {}", utc_text(at), local_text(at)),
                            None => lang.sel("none", "없음").to_string(),
                        },
                    ),
                ]))
            },
        ))
        (components::panel(
            html! { h3 class="panel__title" { "Confirm" } },
            html! {
                form method="post" action=(route::DELETE_PATH) class="cfg-form" {
                    input type="hidden" name=(route::FIELD_ID) value=(schedule.id.to_string());
                    div class="field" {
                        label class="field__label mono" for="s-confirm" {
                            (lang.sel("Type the profile name to confirm", "확인을 위해 프로파일 이름을 입력하세요"))
                        }
                        input id="s-confirm" type="text" name=(route::FIELD_CONFIRM)
                            required autocomplete="off";
                        p class="field__hint mono" { (schedule.profile) }
                    }
                    div class="actions" {
                        button type="submit" { (lang.sel("Delete this schedule", "이 스케줄 삭제")) }
                        " "
                        a href=(route::SCHEDULE_PATH) { (lang.sel("Cancel", "취소")) }
                    }
                }
            },
        ))
    }
}

// ---------------------------------------------------------------------------
// 알림
// ---------------------------------------------------------------------------

/// 검증 실패 알림(400과 함께 나간다).
pub fn problem_notice(lang: Lang, message: &str, hint: Option<&str>) -> Markup {
    components::notice(
        Level::Error,
        lang.sel("Could not save the schedule", "스케줄을 저장할 수 없습니다"),
        html! {
            p { (message) }
            @if let Some(hint) = hint { p { (hint) } }
        },
    )
}

/// 저장 성공 알림.
pub fn saved_notice(lang: Lang, next: Option<DateTime<Utc>>) -> Markup {
    components::notice(
        Level::Ok,
        lang.sel("Schedule saved", "스케줄을 저장했습니다"),
        html! {
            @match next {
                Some(at) => p {
                    (lang.sel("Next run: ", "다음 실행: "))
                    span class="mono" { (utc_text(at)) }
                    " / "
                    span class="muted" { (local_text(at)) }
                }
                None => p { (lang.sel(
                    "This schedule is paused, so it will not run until you enable it.",
                    "이 스케줄은 일시 중지 상태이므로 활성화할 때까지 실행되지 않습니다.",
                )) }
            }
        },
    )
}

/// 삭제 성공 알림.
pub fn deleted_notice(lang: Lang, profile: &str) -> Markup {
    components::notice(
        Level::Warn,
        lang.sel("Schedule deleted", "스케줄을 삭제했습니다"),
        html! {
            p {
                (lang.sel(
                    "No further backups will be started automatically for profile ",
                    "다음 프로파일의 자동 백업이 더 이상 시작되지 않습니다: ",
                ))
                span class="mono" { (profile) }
            }
        },
    )
}

/// 잘못된 스케줄 id 화면(400).
pub fn malformed_id(lang: Lang) -> Markup {
    components::notice(
        Level::Error,
        lang.sel("Bad schedule link", "잘못된 스케줄 링크"),
        html! {
            p { (lang.sel(
                "That schedule id is not a valid identifier. Go back to the list and try again.",
                "그 스케줄 id는 올바른 식별자가 아닙니다. 목록으로 돌아가 다시 시도하세요.",
            )) }
            p { a href=(route::SCHEDULE_PATH) { (lang.sel("Back to schedules", "스케줄 목록으로")) } }
        },
    )
}

/// 없는 스케줄 화면(404).
pub fn not_found(lang: Lang) -> Markup {
    components::notice(
        Level::Warn,
        lang.sel("No such schedule", "그런 스케줄이 없습니다"),
        html! {
            p { (lang.sel(
                "It may have been deleted in another tab.",
                "다른 탭에서 이미 삭제되었을 수 있습니다.",
            )) }
            p { a href=(route::SCHEDULE_PATH) { (lang.sel("Back to schedules", "스케줄 목록으로")) } }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::schedule::cron::CronExpr;
    use crate::web::state::schedules::ScheduleId;

    fn at(rfc3339: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(rfc3339)
            .unwrap()
            .with_timezone(&Utc)
    }

    fn schedule(expr: &str, enabled: bool) -> Schedule {
        Schedule {
            id: ScheduleId::generate(),
            profile: "prod".to_string(),
            expr: CronExpr::parse(expr, Lang::En).unwrap(),
            kind: ScheduleKind::Full,
            enabled,
            created_at: at("2026-07-01T00:00:00Z"),
            last_run_at: None,
            last_job_id: None,
        }
    }

    /// 목록은 UTC 기준을 **세 곳에서** 말한다(모듈 헤더 1·2·3번).
    #[test]
    fn the_screen_states_that_times_are_utc() {
        let items = vec![schedule("0 3 * * *", true)];
        let out = list_body(
            Lang::En,
            &items,
            &LoadReport::default(),
            &CrontabScan::default(),
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(out.contains("Cron (UTC)"), "표 헤더에 기준이 없다");
        assert!(out.contains("Times are UTC"), "상단 안내가 없다");
        assert!(
            out.contains("03:00 UTC"),
            "다음 실행 시각의 UTC 표기가 없다: {out}"
        );

        // 폼에도 라벨과 안내가 있다.
        let form = form_body(
            Lang::En,
            FormMode::New,
            &FormValues::default(),
            &[],
            None,
            None,
        )
        .into_string();
        assert!(form.contains("Cron (UTC)"), "폼 라벨에 기준이 없다");
        assert!(form.contains("Times are UTC"));
    }

    /// 활성인데 발화할 시각이 없는 스케줄은 활성과 **다른 배지**로 보인다 — 조용한 고장을
    /// 정상으로 그리지 않는다.
    #[test]
    fn an_expression_that_never_matches_is_not_shown_as_active() {
        // 2월 30일은 존재하지 않는다.
        let items = vec![schedule("0 0 30 2 *", true)];
        let out = list_body(
            Lang::En,
            &items,
            &LoadReport::default(),
            &CrontabScan::default(),
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(out.contains(">NEVER<"), "발화 불가 배지가 없다: {out}");
        assert!(!out.contains(">ACTIVE<"), "정상으로 그렸다: {out}");
        assert!(
            out.contains(r#"data-level="fail""#),
            "레벨이 실려 있지 않다: {out}"
        );
    }

    /// 일시 중지는 다음 실행 시각을 보여주지 않는다(보여주면 돌 것처럼 읽힌다).
    #[test]
    fn a_paused_schedule_shows_no_next_run() {
        let items = vec![schedule("0 3 * * *", false)];
        let out = list_body(
            Lang::En,
            &items,
            &LoadReport::default(),
            &CrontabScan::default(),
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(out.contains(">PAUSED<"));
        assert!(
            !out.contains("03:00 UTC"),
            "중지 상태인데 다음 실행을 그렸다: {out}"
        );
    }

    /// 손상이면 **목록보다 먼저** 그 사실이 나온다(빈 목록과 구분된다).
    #[test]
    fn a_degraded_load_is_announced_before_the_list() {
        let report = LoadReport {
            file_present: true,
            fatal_error: Some("JSON을 해석할 수 없습니다".to_string()),
            corrupt_copy_saved: true,
            ..LoadReport::default()
        };
        let out = list_body(
            Lang::En,
            &[],
            &report,
            &CrontabScan::default(),
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        let banner = out.find("could not be read").expect("손상 배너가 없다");
        let list = out.find("No schedules yet").expect("목록 문구가 없다");
        assert!(banner < list, "손상 배너가 목록 뒤에 있다");
        assert!(
            out.contains(r#"data-level="error""#),
            "치명적 손상 레벨이 아니다"
        );

        // 항목 일부만 깨진 경우는 문구와 레벨이 다르다.
        let partial = LoadReport {
            file_present: true,
            loaded: 3,
            unreadable_entries: 2,
            ..LoadReport::default()
        };
        let out = degraded_banner(Lang::En, &partial).into_string();
        assert!(out.contains("Some schedule entries"), "{out}");
        assert!(out.contains(r#"data-level="fail""#));
    }

    /// crontab 충돌은 경고로 나오고 항목이 보인다.
    #[test]
    fn crontab_conflicts_are_surfaced() {
        let scan = CrontabScan {
            entries: vec!["0 3 * * * x-backup backup --profile prod".to_string()],
            unavailable: None,
        };
        let out = list_body(
            Lang::En,
            &[],
            &LoadReport::default(),
            &scan,
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(out.contains("crontab still has"), "{out}");
        assert!(out.contains("--profile prod"));
        assert!(out.contains("exits 5"), "락 동작을 설명하지 않는다");

        // 충돌이 없으면 아무 말도 하지 않는다(잡음 금지).
        let quiet = CrontabScan {
            entries: Vec::new(),
            unavailable: Some("crontab 없음".to_string()),
        };
        let out = list_body(
            Lang::En,
            &[],
            &LoadReport::default(),
            &quiet,
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(!out.contains("crontab still has"), "조용해야 한다: {out}");
        assert!(
            !out.contains("crontab 없음"),
            "내부 진단 문구가 새어 나왔다"
        );
    }

    /// 놓친 실행은 **한 번이라도 돈 스케줄에만** 표시된다(모듈 헤더).
    #[test]
    fn missed_runs_only_show_after_a_first_run() {
        let now = at("2026-07-25T00:00:00Z");

        // 아직 돈 적이 없다 — 등록이 아주 오래됐어도 "놓쳤다"고 말하지 않는다.
        let never = schedule("0 3 * * *", true);
        let out = last_run_cell(Lang::En, &never, now).into_string();
        assert!(out.contains("never run yet"), "{out}");
        assert!(
            !out.contains("MISSED"),
            "돈 적 없는 스케줄에 gap을 표시했다: {out}"
        );

        // 한 번 돈 뒤 5일이 지났다 = 5번 놓쳤다.
        let mut stale = schedule("0 3 * * *", true);
        stale.last_run_at = Some(at("2026-07-20T03:00:00Z"));
        let out = last_run_cell(Lang::En, &stale, now).into_string();
        assert!(out.contains("MISSED"), "gap을 표시하지 않았다: {out}");
        assert!(
            out.contains(">4<") || out.contains(">5<"),
            "개수가 없다: {out}"
        );

        // 일시 중지 스케줄은 gap을 세지 않는다(돌 리가 없으므로 gap이 아니다).
        stale.enabled = false;
        let out = last_run_cell(Lang::En, &stale, now).into_string();
        assert!(
            !out.contains("MISSED"),
            "중지 스케줄에 gap을 표시했다: {out}"
        );
    }

    /// 놓친 실행 수가 상한이면 `+`로 정직하게 표기한다.
    #[test]
    fn capped_missed_count_is_marked() {
        assert_eq!(missed_text(7), "7");
        assert_eq!(
            missed_text(crate::web::state::schedules::MISSED_SCAN_MAX),
            format!("{}+", crate::web::state::schedules::MISSED_SCAN_MAX)
        );
    }

    /// 삭제 확인 화면은 "조용히 멈춘다"는 성질을 명시하고 일시 중지 대안을 제시한다.
    #[test]
    fn delete_confirmation_explains_the_silent_consequence() {
        let out = delete_confirm_body(
            Lang::En,
            &schedule("0 3 * * *", true),
            at("2026-07-25T00:00:00Z"),
            None,
        )
        .into_string();
        assert!(
            out.contains("silently"),
            "조용한 결과를 설명하지 않는다: {out}"
        );
        assert!(out.contains("uncheck `enabled`"), "일시 중지 대안이 없다");
        assert!(out.contains(route::FIELD_CONFIRM), "확인 입력이 없다");
        assert!(out.contains("prod"), "무엇을 지우는지 보이지 않는다");
    }

    /// 검증 실패 후에도 사용자가 적은 값이 폼에 남는다.
    #[test]
    fn a_failed_submission_keeps_what_the_operator_typed() {
        let values = FormValues {
            id: None,
            profile: "staging".to_string(),
            expr: "60 * * * *".to_string(),
            kind: Some(ScheduleKind::Incr),
            enabled: true,
        };
        let out = form_body(
            Lang::En,
            FormMode::New,
            &values,
            &["prod".to_string(), "staging".to_string()],
            None,
            Some(problem_notice(
                Lang::En,
                "분 필드의 60이 범위를 벗어났습니다",
                None,
            )),
        )
        .into_string();
        assert!(
            out.contains(r#"value="60 * * * *""#),
            "표현식이 사라졌다: {out}"
        );
        assert!(
            out.contains(r#"<option value="staging" selected>"#),
            "프로파일 선택이 사라졌다"
        );
        assert!(
            out.contains(r#"<option value="incr" selected>"#),
            "종류 선택이 사라졌다"
        );
        assert!(out.contains("범위를 벗어났습니다"), "오류 문구가 없다");
        assert!(out.contains("checked"), "활성 체크가 사라졌다");
    }

    /// 마크업에 색 리터럴·인라인 스타일이 없다([`components`] 계약).
    #[test]
    fn markup_carries_no_presentation() {
        let mut item = schedule("0 3 * * *", true);
        item.last_run_at = Some(at("2026-07-20T03:00:00Z"));
        item.last_job_id = Some(crate::web::state::jobs::JobId::generate());
        let report = LoadReport {
            file_present: true,
            loaded: 1,
            unreadable_entries: 1,
            corrupt_copy_saved: true,
            recovered_from_prev: true,
            fatal_error: Some("x".to_string()),
        };
        let scan = CrontabScan {
            entries: vec!["0 3 * * * x-backup backup --profile prod".to_string()],
            unavailable: None,
        };
        let now = at("2026-07-25T00:00:00Z");
        let rendered = [
            list_body(
                Lang::En,
                std::slice::from_ref(&item),
                &report,
                &scan,
                now,
                None,
            )
            .into_string(),
            form_body(
                Lang::Ko,
                FormMode::Edit,
                &FormValues::from_schedule(&item),
                &["prod".to_string()],
                Some(now),
                None,
            )
            .into_string(),
            delete_confirm_body(Lang::Ko, &item, now, None).into_string(),
            saved_notice(Lang::En, Some(now)).into_string(),
            deleted_notice(Lang::En, "prod").into_string(),
            malformed_id(Lang::En).into_string(),
            not_found(Lang::En).into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
        }
    }

    /// 적대적 문자열은 전부 이스케이프된다 — 프로파일명·표현식은 사람이 적은 값이다.
    #[test]
    fn hostile_strings_are_escaped() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let mut item = schedule("0 3 * * *", true);
        item.profile = hostile.to_string();
        let now = at("2026-07-25T00:00:00Z");
        let rendered = [
            list_body(
                Lang::En,
                std::slice::from_ref(&item),
                &LoadReport::default(),
                &CrontabScan {
                    entries: vec![hostile.to_string()],
                    unavailable: None,
                },
                now,
                None,
            )
            .into_string(),
            delete_confirm_body(Lang::En, &item, now, None).into_string(),
            deleted_notice(Lang::En, hostile).into_string(),
            problem_notice(Lang::En, hostile, Some(hostile)).into_string(),
            form_body(
                Lang::En,
                FormMode::New,
                &FormValues {
                    profile: hostile.to_string(),
                    expr: hostile.to_string(),
                    ..FormValues::default()
                },
                &[],
                None,
                None,
            )
            .into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
            assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
        }
    }
}
