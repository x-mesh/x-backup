//! 잡 이력 화면의 마크업 — 목록과 상세.
//!
//! ## 이 파일은 결과→레벨 판정을 하지 않는다 — [`job_exit::level_for_label`]을 부른다
//! 잡의 결과 어휘는 t10이 정한다([`crate::web::job::JobOutcome::label`]) — `succeeded`,
//! `succeeded-with-warnings`, `lock-conflict`, `failed`, `rejected`, `precheck-failed`,
//! `unexpected-exit`, `signaled`, `unknown`. 저장소는 그 문자열을 그대로 남기고
//! ([`crate::web::state::jobs`]), 이 파일은 그것을 화면 레벨([`Level`])로 옮겨야 한다.
//!
//! 한동안 그 매핑(`level_for_outcome`)을 **이 파일이 자기 사본으로** 들고 있었다. 원래
//! 계획은 "t13이 표현을 한 곳에 모을 때 이관한다"였는데, 이관되기 전에 두 사본이 실제로
//! 갈라졌다 — exit 2(`rejected`)와 `unexpected-exit`가 t13에서는 [`Level::Error`], 여기서는
//! [`Level::Fail`]이어서 같은 잡이 화면마다 다른 색으로 보였다. 그래서 사본을 지우고
//! [`job_exit::level_for_label`](crate::web::job::exit::level_for_label) 하나만 부른다.
//! 그 함수의 doc이 각 레벨의 근거를 들고 있고, 그중 이 화면에 가장 중요한 두 줄은 여전히
//! 이것이다:
//!
//! - **exit 4(`succeeded-with-warnings`)는 성공이다.** [`Level::Fail`]로 칠하면 운영자가
//!   멀쩡한 백업을 다시 돌리거나, 반대로 경고를 흘려보낸다.
//! - **exit 5(`lock-conflict`)는 실패가 아니다.** 아무 일도 일어나지 않았고, 그대로 다시
//!   시도하면 된다. 실패로 칠하면 고칠 것이 없는데 원인을 찾아 헤매게 된다.
//!
//! 문구([`outcome_headline`]·[`outcome_detail`])는 여기 남는다. 이력 화면은 "지난 잡을
//! 훑어보는" 문맥이고 t13의 [`job_exit::present`](crate::web::job::exit::present)는 "방금
//! 끝난 잡 하나를 설명하는" 문맥이라 같은 결과에 대해 서로 다른 분량·어투가 맞다. 갈라지면
//! 사고가 나는 것은 **레벨**(색)이지 문장이 아니다 — 색은 운영자가 글을 읽기 전에 먼저
//! 믿어버리는 신호이기 때문이다.
//!
//! ## 모양은 담지 않는다
//! [`components`] 헤더의 계약을 그대로 따른다 — 상태는 `data-level` 토큰으로만 말하고,
//! 색·인라인 스타일 리터럴은 마크업에 없다. 아래 `markup_carries_no_presentation`
//! 테스트가 그것을 고정한다. 새 CSS 클래스를 만들지 않고 이미 있는 것만 쓴다
//! (`.dtable`·`.logdump`·`.tag`·`.card`·`.mono`) — 아이덴티티가 확정되지 않은 상태에서
//! 화면마다 클래스를 늘리면 그 확정이 화면 수만큼 번진다.
//!
//! ## 남의 텍스트를 다루는 두 가지 규칙
//! 로그 본문과 인자는 자식 프로세스가 만든 문자열이다. 그래서:
//!
//! 1. **마스킹을 무조건 경유한다.** 쓰는 쪽(t11)이 이미 지웠더라도 여기서 한 번 더
//!    [`SecretRegistry::mask`]를 통과시킨다. 저장소는 내용을 검사하지 않으므로
//!    ([`crate::web::state::jobs`] 헤더), 화면이 마지막 방어선이다.
//! 2. **길이를 자른다.** 자식이 20,000자짜리 한 줄을 찍으면 브라우저가 그 줄 하나로
//!    레이아웃을 잃는다. [`MAX_TEXT_CHARS`]로 문자 경계에서 자른다(바이트로 자르면
//!    한글이 깨진다).
//!
//! 이스케이프는 maud의 자동 처리에 맡긴다 — 이 파일에는 `PreEscaped`가 한 번도 없다.

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::job::exit as job_exit;
use crate::web::mask::SecretRegistry;
use crate::web::routes::jobs::{job_detail_href, JOBS_TITLE};
use crate::web::state::jobs::{JobDetail, JobHistory, JobSummary};
use crate::web::view::components::{self, Level};

/// 로그 한 줄·인자 하나를 화면에 그릴 때의 문자 수 상한.
///
/// 2,000자는 어떤 진단 메시지도(스택 힌트가 붙은 mongodump 오류까지) 온전히 들어가는
/// 폭이면서, 한 줄이 화면을 통째로 먹지 않는 선이다. 저장된 원문은 자르지 않는다 —
/// 자르는 것은 표시뿐이고, 잘렸다는 사실은 말줄임표로 보인다.
pub const MAX_TEXT_CHARS: usize = 2_000;

/// 시각 표기 형식 — **항상 UTC**.
///
/// 브라우저·서버·자식 프로세스의 시간대가 서로 다를 수 있다. 화면이 로컬 시간대로
/// 바꿔 보여주면 운영자가 자식 로그(UTC)와 대조할 때 시각이 어긋난다. `Z`를 붙여
/// "이건 UTC다"를 글자로 말한다.
const TIMESTAMP_FORMAT: &str = "%Y-%m-%d %H:%M:%SZ";

/// 결과 한 줄 설명(설명 문장이므로 ko/en 토글).
fn outcome_headline(lang: Lang, outcome: &str) -> String {
    match outcome {
        "succeeded" => lang.sel("Finished successfully.", "성공으로 끝났습니다."),
        "succeeded-with-warnings" => lang.sel(
            "Finished successfully, with warnings — the backup exists.",
            "경고와 함께 성공으로 끝났습니다 — 산출물은 있습니다.",
        ),
        "lock-conflict" => lang.sel(
            "Nothing ran: another instance held the profile lock.",
            "아무것도 실행되지 않았습니다: 같은 프로파일의 다른 인스턴스가 락을 들고 있었습니다.",
        ),
        "failed" => lang.sel(
            "The job started and then failed.",
            "작업이 시작된 뒤 실패했습니다.",
        ),
        "rejected" => lang.sel(
            "Rejected before starting — the argument combination was not valid.",
            "시작 전에 거부됐습니다 — 인자 조합이 유효하지 않았습니다.",
        ),
        "precheck-failed" => lang.sel(
            "Precheck failed, so the job never started.",
            "사전 점검이 실패해 작업이 시작되지 않았습니다.",
        ),
        "unexpected-exit" => lang.sel(
            "The child exited with a code this build does not define.",
            "자식 프로세스가 이 빌드가 정의하지 않은 코드로 끝났습니다.",
        ),
        "signaled" => lang.sel(
            "Killed by a signal — output may be half written.",
            "시그널로 종료됐습니다 — 산출물이 반쯤 남았을 수 있습니다.",
        ),
        _ => lang.sel(
            "The result of this job is unknown.",
            "이 잡의 결과를 알 수 없습니다.",
        ),
    }
    .to_string()
}

/// 결과의 다음 행동(있으면). 배너 두 번째 줄에 들어간다.
fn outcome_detail(lang: Lang, outcome: &str) -> Option<String> {
    let text = match outcome {
        "succeeded" => return None,
        "succeeded-with-warnings" => lang.sel(
            "exit 4 is success with caveats: do not re-run it just because of the colour. Read the warnings in the log below.",
            "exit 4는 단서가 붙은 성공입니다. 색만 보고 다시 돌리지 마세요 — 아래 로그의 경고를 읽으세요.",
        ),
        "lock-conflict" => lang.sel(
            "exit 5 needs no fix: retry once the other run finishes. A cron-launched CLI shares the same lock.",
            "exit 5는 고칠 것이 없습니다: 다른 실행이 끝난 뒤 다시 시도하세요. cron으로 띄운 CLI도 같은 락을 공유합니다.",
        ),
        "rejected" | "precheck-failed" => lang.sel(
            "Nothing was written, so the data is untouched.",
            "아무것도 쓰이지 않았으므로 데이터는 그대로입니다.",
        ),
        "signaled" => lang.sel(
            "Check whether the output was cleaned up before retrying.",
            "다시 시도하기 전에 산출물이 정리됐는지 확인하세요.",
        ),
        _ => return None,
    };
    Some(text.to_string())
}

/// 목록 화면 본문.
pub fn list_body(lang: Lang, history: &JobHistory, registry: &SecretRegistry) -> Markup {
    let subtitle = lang.sel(
        "Job history recorded by this console. It is a cache: the backups themselves live in the destination.",
        "이 콘솔이 기록한 잡 이력입니다. 캐시이므로 백업 자체는 destination에 있습니다.",
    );
    html! {
        (components::page_head(JOBS_TITLE, Some(subtitle)))
        @if history.unreadable_lines > 0 {
            (partial_history_notice(lang, history.unreadable_lines))
        }
        @if history.entries.is_empty() {
            (empty_history(lang, history.index_present))
        } @else {
            (history_table(lang, &history.entries, registry))
        }
    }
}

/// 상세 화면 본문.
pub fn detail_body(lang: Lang, detail: &JobDetail, registry: &SecretRegistry) -> Markup {
    let title = format!("{JOBS_TITLE} · {}", detail.id.short());
    html! {
        (components::page_head(&title, Some(&detail.id.to_string())))
        @match &detail.summary {
            Some(summary) => {
                (status_block(lang, summary))
                (summary_meta(lang, summary))
                (args_panel(lang, summary, registry))
            }
            None => {
                (components::notice(Level::Error, lang.sel("Unknown job", "알 수 없는 잡"), html! {
                    p { (lang.sel(
                        "No history was found for this job id. It may have been recorded by a different state directory.",
                        "이 잡 id에 대한 이력을 찾지 못했습니다. 다른 state 디렉터리에 기록된 잡일 수 있습니다.",
                    )) }
                }))
            }
        }
        (log_panel(lang, detail, registry))
        (nav_back(lang))
    }
}

/// 잡 id 형식이 틀렸을 때의 본문(400과 함께 나간다).
///
/// **입력 문자열을 되돌려 그리지 않는다.** 남의 문자열을 반사하는 화면을 만들지 않는 것이
/// 기본이고(이스케이프가 있어도 반사 자체를 습관으로 만들지 않는다), 여기서는 진단에도
/// 도움이 되지 않는다 — 사용자가 알아야 할 것은 "이 링크가 잡 id 형식이 아니다"뿐이다.
pub fn malformed_id(lang: Lang) -> Markup {
    html! {
        (components::page_head(JOBS_TITLE, None))
        (components::notice(Level::Error, lang.sel("Malformed job id", "잡 id 형식 오류"), html! {
            p { (lang.sel(
                "That link does not carry a job id. Job ids are UUIDs, so open the job from the history list instead.",
                "이 링크에는 잡 id가 없습니다. 잡 id는 UUID이므로 이력 목록에서 잡을 여세요.",
            )) }
        }))
        (nav_back(lang))
    }
}

/// 결과 배너 — 끝난 잡은 판정 배너, 도는 잡은 카드.
///
/// 도는 잡에 [`components::verdict_banner`]를 쓰지 않는 이유: 그 배너는 네 [`Level`] 중
/// 하나를 요구하는데 "아직 모른다"는 그중에 없다. 억지로 [`Level::Ok`]를 주면 화면이
/// 끝나지도 않은 잡을 성공으로 말한다.
fn status_block(lang: Lang, summary: &JobSummary) -> Markup {
    match summary.outcome.as_deref() {
        Some(outcome) => components::verdict_banner(
            job_exit::level_for_label(outcome),
            &outcome_headline(lang, outcome),
            outcome_detail(lang, outcome).as_deref(),
        ),
        None => components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("In progress", "진행 중")) }
                span class="tag" { "running" }
            },
            html! {
                p { (lang.sel(
                    "No end record yet. Either the job is still running, or the console stopped before it could write one.",
                    "종료 기록이 아직 없습니다. 잡이 계속 돌고 있거나, 콘솔이 기록을 남기기 전에 멈춘 것입니다.",
                )) }
            },
        ),
    }
}

/// 상세 상단 메타 — 라벨은 영문 기술용어 고정.
fn summary_meta(lang: Lang, summary: &JobSummary) -> Markup {
    let dash = "-".to_string();
    components::meta_list(&[
        ("command", summary.command.clone()),
        (
            "profile",
            summary.profile.clone().unwrap_or_else(|| dash.clone()),
        ),
        ("started", format_ts(summary.started_at)),
        (
            "finished",
            summary
                .finished_at
                .map(format_ts)
                .unwrap_or_else(|| lang.sel("(running)", "(진행 중)").to_string()),
        ),
        ("duration", duration_text(lang, summary)),
        (
            "outcome",
            summary.outcome.clone().unwrap_or_else(|| dash.clone()),
        ),
        ("exit", exit_text(summary)),
        // pid는 운영자가 `ps`로 대조할 값이다(경로 같은 서버 내부 구조가 아니다).
        ("pid", summary.pid.map(|p| p.to_string()).unwrap_or(dash)),
    ])
}

/// 인자 패널 — 자식에게 넘어간 argv(전역 플래그 제외).
fn args_panel(lang: Lang, summary: &JobSummary, registry: &SecretRegistry) -> Markup {
    let joined = summary
        .args_masked
        .iter()
        .map(|arg| truncate(&registry.mask(arg)))
        .collect::<Vec<_>>()
        .join(" ");
    components::panel(
        html! {
            h3 class="panel__title" { "Arguments" }
            span class="counts" { (summary.args_masked.len()) " args" }
        },
        html! {
            @if joined.is_empty() {
                p class="muted" { (lang.sel("No arguments were recorded.", "기록된 인자가 없습니다.")) }
            } @else {
                pre class="logdump" { (joined) }
            }
            p class="muted" {
                (lang.sel(
                    "Global flags (--config, --lang) are added by the runner and are not shown here.",
                    "전역 플래그(--config, --lang)는 러너가 붙이므로 여기에는 나오지 않습니다.",
                ))
            }
        },
    )
}

/// 로그 패널 — 본문은 `.logdump`(높이 제한 + 자체 스크롤)를 재사용한다.
fn log_panel(lang: Lang, detail: &JobDetail, registry: &SecretRegistry) -> Markup {
    components::panel(
        html! {
            h3 class="panel__title" { "Log" }
            span class="counts" { (detail.logs.len()) " lines" }
        },
        html! {
            @if detail.dropped_leading_logs > 0 {
                p class="muted" {
                    (lang.sel("Earlier lines omitted: ", "앞부분 생략된 줄: "))
                    (detail.dropped_leading_logs)
                }
            }
            @if detail.unreadable_lines > 0 {
                (partial_history_notice(lang, detail.unreadable_lines))
            }
            @if detail.logs.is_empty() {
                p class="muted" {
                    @if detail.log_file_present {
                        (lang.sel("No log lines were recorded for this job.", "이 잡에 기록된 로그 줄이 없습니다."))
                    } @else {
                        (lang.sel(
                            "The log file is gone — it was most likely removed by rotation. The summary above is kept.",
                            "로그 파일이 없습니다 — 로테이션으로 정리된 것으로 보입니다. 위의 요약은 남아 있습니다.",
                        ))
                    }
                }
            } @else {
                pre class="logdump" {
                    @for line in &detail.logs {
                        (line.ts.format("%H:%M:%S").to_string())
                        " "
                        (line.stream.token())
                        " "
                        (truncate(&registry.mask(&line.text)))
                        "\n"
                    }
                }
            }
        },
    )
}

/// 목록으로 돌아가는 링크. 브라우저 뒤로가기에만 의존하지 않는다(새 탭으로 열린 상세에서는
/// 뒤로 갈 곳이 없다).
fn nav_back(lang: Lang) -> Markup {
    html! {
        p class="muted" {
            a href=(crate::web::routes::jobs::JOBS_PATH) {
                (lang.sel("Back to job history", "잡 이력으로 돌아가기"))
            }
        }
    }
}

/// 이력 표 — 행에도 `data-level`을 실어 CSS가 행 전체를 물들일 수 있게 한다(doctor의
/// 항목 표와 같은 방식: 배지 하나만으로는 행이 20개를 넘어가면 눈이 상태를 잃는다).
fn history_table(lang: Lang, entries: &[JobSummary], registry: &SecretRegistry) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "Started" }
                        th scope="col" { "Job" }
                        th scope="col" { "Command" }
                        th scope="col" { "Profile" }
                        th scope="col" { "Exit" }
                        th scope="col" { "Duration" }
                    }
                }
                tbody {
                    @for entry in entries {
                        @match entry.outcome.as_deref() {
                            Some(outcome) => {
                                @let level = job_exit::level_for_label(outcome);
                                tr data-level=(level.token()) {
                                    td { (components::badge(level)) }
                                    (row_tail(lang, entry, registry))
                                }
                            }
                            // 도는 잡에는 레벨이 없다. `.tag`는 레벨 색을 쓰지 않는 중립
                            // 꼬리표이므로(`app.css` 주석) "판정 없음"에 정확히 맞는다 —
                            // 배지를 쓰면 네 레벨 중 하나를 고르게 되고, 그건 거짓말이다.
                            None => {
                                tr {
                                    td { span class="tag" { "running" } }
                                    (row_tail(lang, entry, registry))
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// 표 한 행의 상태 열을 제외한 나머지 — 상태 열만 두 갈래이므로 그 뒤를 공유한다.
fn row_tail(lang: Lang, entry: &JobSummary, registry: &SecretRegistry) -> Markup {
    html! {
        td class="mono" { (format_ts(entry.started_at)) }
        // 링크는 단축 id에 붙인다 — 표에서 가장 짧으면서 그 잡을 특정하는 열이다.
        td class="mono key" { a href=(job_detail_href(&entry.id)) { (entry.id.short()) } }
        td { (entry.command) }
        td class="mono" {
            @match &entry.profile {
                Some(profile) => (registry.mask(profile)),
                None => "-",
            }
        }
        td class="num" {
            @match entry.exit_code {
                Some(code) => (code.to_string()),
                None => "-",
            }
        }
        // `.age`가 이미 등폭·tabular-nums·nowrap을 준다 — `.num`을 겹쳐 쓰지 않는다.
        td class="age" { (duration_text(lang, entry)) }
    }
}

/// 이력이 하나도 없을 때. 상태가 아니라 부재이므로 판정 배너·알림을 쓰지 않는다.
fn empty_history(lang: Lang, index_present: bool) -> Markup {
    html! {
        div class="card" {
            p { (lang.sel(
                "No job history yet. Jobs started from this console appear here.",
                "잡 이력이 아직 없습니다. 이 콘솔에서 시작한 잡이 여기 나타납니다.",
            )) }
            @if !index_present {
                p class="muted" { (lang.sel(
                    "The history index file does not exist yet — it is created when the first job runs.",
                    "이력 인덱스 파일이 아직 없습니다 — 첫 잡이 돌 때 만들어집니다.",
                )) }
            }
        }
    }
}

/// 일부 줄을 읽지 못했음을 알린다. 손상을 조용히 삼키지 않기 위한 블록이다.
fn partial_history_notice(lang: Lang, unreadable: usize) -> Markup {
    components::notice(
        Level::Warn,
        lang.sel("Partial history", "일부만 읽음"),
        html! {
            p {
                (lang.sel("Lines that could not be read: ", "읽지 못한 줄: "))
                (unreadable)
                ". "
                (lang.sel(
                    "A truncated last line is the usual cause — a crash while writing. Everything else is intact.",
                    "쓰던 중 크래시로 마지막 줄이 잘린 경우가 흔한 원인입니다. 나머지는 온전합니다.",
                ))
            }
        },
    )
}

/// 시각 표기(UTC 고정 — [`TIMESTAMP_FORMAT`]).
fn format_ts(ts: chrono::DateTime<chrono::Utc>) -> String {
    ts.format(TIMESTAMP_FORMAT).to_string()
}

/// 기간 칸 문자열. 끝나지 않은 잡은 `-`(경과 시간을 여기서 계산하면 같은 잡이 새로고침마다
/// 달라 보이고, 그건 진행률 화면(t11)의 일이다).
fn duration_text(lang: Lang, summary: &JobSummary) -> String {
    match summary.duration() {
        Some(delta) => format_duration(lang, delta),
        None => "-".to_string(),
    }
}

/// 종료 코드 칸. 시그널 종료는 코드가 없으므로 결과 어휘를 그대로 보여준다.
fn exit_text(summary: &JobSummary) -> String {
    match (summary.exit_code, summary.outcome.as_deref()) {
        (Some(code), _) => code.to_string(),
        (None, Some(outcome)) => outcome.to_string(),
        (None, None) => "-".to_string(),
    }
}

/// 기간을 큰 단위 두 개로 표기한다("45s", "2m 05s", "1h 03m").
///
/// 단위 표기는 짧은 기술 표기이므로 언어별로 둔다(`status` 핸들러의 `format_age_secs`와
/// 같은 어휘). 1초 미만은 밀리초로 보여준다 — `list`·`status`처럼 순식간에 끝나는 잡이
/// 전부 "0s"로 뭉개지면 그 열이 아무 정보를 주지 않는다.
fn format_duration(lang: Lang, delta: chrono::TimeDelta) -> String {
    let total = delta.num_seconds();
    if total < 0 {
        // 시계 되감기·수동 편집. 억지로 0으로 접지 않고 이상하다고 말한다.
        return "?".to_string();
    }
    if total == 0 {
        return format!("{}{}", delta.num_milliseconds(), lang.sel("ms", "밀리초"));
    }
    if total < 60 {
        return format!("{total}{}", lang.sel("s", "초"));
    }
    if total < 3_600 {
        return format!(
            "{}{} {:02}{}",
            total / 60,
            lang.sel("m", "분"),
            total % 60,
            lang.sel("s", "초")
        );
    }
    format!(
        "{}{} {:02}{}",
        total / 3_600,
        lang.sel("h", "시간"),
        (total % 3_600) / 60,
        lang.sel("m", "분")
    )
}

/// 문자열을 [`MAX_TEXT_CHARS`]로 자른다. **문자 경계**로 자르므로 멀티바이트가 깨지지
/// 않는다(`routes::doctor::excerpt`와 같은 처리).
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_TEXT_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_TEXT_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::job::JobOutcome;
    use crate::web::state::jobs::{JobId, JobLogLine, LogStream};
    use chrono::{TimeZone, Utc};

    /// 고정 시각 — 스냅샷 단정이 지금 시각에 흔들리지 않게 한다.
    fn ts(offset_secs: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_785_000_000 + offset_secs, 0).unwrap()
    }

    /// 끝난 잡 요약 하나.
    fn finished(outcome: JobOutcome) -> JobSummary {
        JobSummary {
            id: JobId::generate(),
            command: "backup".to_string(),
            profile: Some("prod".to_string()),
            args_masked: vec![
                "backup".to_string(),
                "--profile".to_string(),
                "prod".to_string(),
            ],
            started_at: ts(0),
            pid: Some(4242),
            finished_at: Some(ts(75)),
            outcome: Some(outcome.label().to_string()),
            exit_code: outcome.exit_code(),
        }
    }

    /// 도는 잡 요약 하나.
    fn running() -> JobSummary {
        JobSummary {
            id: JobId::generate(),
            command: "restore".to_string(),
            profile: Some("staging".to_string()),
            args_masked: Vec::new(),
            started_at: ts(0),
            pid: Some(31337),
            finished_at: None,
            outcome: None,
            exit_code: None,
        }
    }

    fn detail_of(summary: JobSummary, logs: Vec<JobLogLine>) -> JobDetail {
        JobDetail {
            id: summary.id,
            summary: Some(summary),
            logs,
            dropped_leading_logs: 0,
            log_file_present: true,
            unreadable_lines: 0,
        }
    }

    fn log(text: &str) -> JobLogLine {
        JobLogLine {
            ts: ts(1),
            stream: LogStream::Stderr,
            text: text.to_string(),
        }
    }

    fn history_of(entries: Vec<JobSummary>) -> JobHistory {
        JobHistory {
            entries,
            unreadable_lines: 0,
            index_present: true,
        }
    }

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    // -- 결과 표현 --------------------------------------------------------

    /// **exit 4는 실패가 아니다** — 레벨·글자 양쪽에서 성공임이 드러난다.
    #[test]
    fn exit_four_is_not_shown_as_failure() {
        let outcome = JobOutcome::SucceededWithWarnings;
        let label = outcome.label();
        assert!(outcome.is_success(), "t10 전제가 바뀌었다");
        let level = job_exit::level_for_label(label);
        assert_eq!(level, Level::Warn);
        assert_ne!(level, Level::Fail, "exit 4를 실패로 칠하면 안 된다");
        assert_ne!(level, Level::Error);

        // 화면에도 "성공"이라는 사실이 글자로 있어야 한다(색만으로는 되묻게 된다).
        assert!(outcome_headline(Lang::En, label).contains("success"));
        assert!(outcome_headline(Lang::Ko, label).contains("성공"));
        let detail = detail_body(
            Lang::En,
            &detail_of(finished(outcome), vec![]),
            &empty_registry(),
        )
        .into_string();
        assert!(
            detail.contains(r#"data-level="warn""#),
            "레벨 누락: {detail}"
        );
        assert!(!detail.contains(r#"data-level="fail""#), "실패로 칠해졌다");
        assert!(detail.contains("exit 4 is success"), "설명 누락: {detail}");
    }

    /// **exit 5는 실패가 아니다** — 재시도하면 된다는 사실이 화면에 있다.
    #[test]
    fn exit_five_lock_conflict_is_not_shown_as_failure() {
        let outcome = JobOutcome::LockConflict;
        let label = outcome.label();
        assert!(outcome.retryable(), "t10 전제가 바뀌었다");
        assert_eq!(job_exit::level_for_label(label), Level::Warn);
        assert_ne!(job_exit::level_for_label(label), Level::Fail);

        let en = outcome_detail(Lang::En, label).expect("다음 행동 안내 누락");
        assert!(en.contains("retry"), "재시도 안내 누락: {en}");
        assert!(outcome_detail(Lang::Ko, label)
            .unwrap()
            .contains("다시 시도"));
        let body = detail_body(
            Lang::Ko,
            &detail_of(finished(outcome), vec![]),
            &empty_registry(),
        )
        .into_string();
        assert!(
            !body.contains(r#"data-level="fail""#),
            "락 충돌이 실패로 칠해졌다"
        );
    }

    /// 실패·미상은 각각 fail/error로 갈리고, 모르는 값은 초록이 되지 않는다.
    ///
    /// ## 이 테스트는 예전에 틀린 동작을 고정하고 있었다
    /// 원래 `rejected`(exit 2)와 `unexpected-exit`도 [`Level::Fail`]로 단정했다. 그건 이
    /// 파일이 자기 레벨 사본을 들고 있던 시절의 값이고, t13
    /// (`src/web/job/exit.rs`)은 같은 두 결과를 [`Level::Error`]로 두고 있었다 — 즉 이
    /// 테스트가 "두 화면의 색이 다르다"는 상태를 초록불로 통과시키고 있었다. 정본은 t13이다:
    /// 둘 다 **우리 쪽 문제**(웹이 만든 argv가 CLI 사용법을 위반했다 / 자식이 우리 에러
    /// 모델을 거치지 않고 죽었다)라 운영자가 대상 서버를 뒤져도 원인이 없고, `Fail`("대상을
    /// 고치면 된다")과는 취해야 할 행동이 다르다.
    #[test]
    fn failures_and_unknowns_map_to_distinct_levels() {
        // 대상·데이터 쪽을 고쳐야 하는 결과만 Fail이다.
        for label in ["failed", "precheck-failed"] {
            assert_eq!(job_exit::level_for_label(label), Level::Fail, "{label}");
        }
        // 우리 쪽 문제 + 판정 불가는 Error다.
        for label in [
            "rejected",
            "unexpected-exit",
            "signaled",
            "unknown",
            "",
            "SUCCEEDED",
            "future-outcome",
        ] {
            assert_eq!(
                job_exit::level_for_label(label),
                Level::Error,
                "'{label}'이 조용히 통과했다"
            );
        }
        assert_eq!(job_exit::level_for_label("succeeded"), Level::Ok);
    }

    /// t10이 내놓는 **모든** 결과 어휘에 설명이 있다 — 새 variant가 생기면 여기서 걸린다.
    #[test]
    fn every_t10_outcome_has_a_headline() {
        let all = [
            JobOutcome::Succeeded,
            JobOutcome::Failed,
            JobOutcome::Rejected,
            JobOutcome::PrecheckFailed,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::LockConflict,
            JobOutcome::UnexpectedExit(42),
            JobOutcome::Signaled(15),
            JobOutcome::Unknown,
        ];
        let mut headlines = Vec::new();
        for outcome in all {
            let label = outcome.label();
            let headline = outcome_headline(Lang::En, label);
            assert!(!headline.is_empty(), "{label} 설명이 비었다");
            // 성공/실패 판정이 t10과 어긋나지 않는지 교차 확인한다.
            let level = job_exit::level_for_label(label);
            if outcome.is_success() {
                assert_ne!(level, Level::Fail, "{label}은 성공인데 실패로 칠했다");
                assert_ne!(level, Level::Error, "{label}은 성공인데 미상으로 칠했다");
            }
            headlines.push(headline);
        }
        headlines.sort();
        let count = headlines.len();
        headlines.dedup();
        // `unknown`과 미지 문자열이 같은 설명을 공유하므로 완전 유일까지는 요구하지 않되,
        // 어휘가 하나로 뭉개지지 않았는지는 확인한다.
        assert!(
            headlines.len() >= count - 1,
            "결과 설명이 서로 뭉개졌다: {headlines:?}"
        );
    }

    // -- 목록 ------------------------------------------------------------

    /// 목록은 최신순 그대로 그리고, 상세 링크와 각 열을 낸다.
    #[test]
    fn list_renders_rows_with_links_and_columns() {
        let summary = finished(JobOutcome::Succeeded);
        let href = job_detail_href(&summary.id);
        let short = summary.id.short();
        let out = list_body(Lang::En, &history_of(vec![summary]), &empty_registry()).into_string();
        assert!(
            out.contains(&format!(r#"href="{href}""#)),
            "상세 링크 누락: {out}"
        );
        assert!(out.contains(&short), "단축 id 누락");
        assert!(out.contains("backup"), "명령 누락");
        assert!(out.contains("prod"), "프로파일 누락");
        assert!(out.contains(r#"data-level="ok""#), "상태 레벨 누락");
        assert!(out.contains("1m 15s"), "기간 표기 누락: {out}");
        assert!(out.contains("2026-"), "시각 표기 누락: {out}");
    }

    /// 도는 잡은 배지 대신 `running` 태그를 달고, 종료 코드·기간 칸이 비어 있다.
    #[test]
    fn running_job_is_not_painted_as_a_result() {
        let out =
            list_body(Lang::En, &history_of(vec![running()]), &empty_registry()).into_string();
        assert!(out.contains("running"), "진행 중 표시 누락: {out}");
        assert!(
            !out.contains(r#"data-level="ok""#),
            "끝나지 않은 잡을 성공으로 칠했다: {out}"
        );
        assert!(!out.contains("<td>0</td>"), "없는 종료 코드가 표시됐다");
    }

    /// 이력이 없으면 안내 문장이 나오고 표는 만들지 않는다.
    #[test]
    fn empty_history_renders_placeholder() {
        let out = list_body(
            Lang::En,
            &JobHistory {
                entries: Vec::new(),
                unreadable_lines: 0,
                index_present: false,
            },
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains("No job history yet"), "안내 문장 누락: {out}");
        assert!(
            out.contains("index file does not exist"),
            "인덱스 부재 설명 누락"
        );
        assert!(!out.contains("<table"), "빈 표를 그렸다");
    }

    /// 읽지 못한 줄이 있으면 화면이 그 사실을 말한다(손상을 삼키지 않는다).
    #[test]
    fn unreadable_lines_are_surfaced() {
        let out = list_body(
            Lang::En,
            &JobHistory {
                entries: vec![finished(JobOutcome::Succeeded)],
                unreadable_lines: 3,
                index_present: true,
            },
            &empty_registry(),
        )
        .into_string();
        assert!(
            out.contains("Partial history"),
            "부분 읽기 알림 누락: {out}"
        );
        assert!(out.contains(r#"data-level="warn""#));
        assert!(out.contains("3"), "개수 누락");
    }

    // -- 상세 ------------------------------------------------------------

    /// 상세는 메타·인자·로그를 모두 낸다.
    #[test]
    fn detail_renders_meta_args_and_log() {
        let summary = finished(JobOutcome::Succeeded);
        let full_id = summary.id.to_string();
        let out = detail_body(
            Lang::En,
            &detail_of(summary, vec![log("dumping users"), log("done")]),
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains(&full_id), "전체 id 누락");
        assert!(out.contains("--profile prod"), "인자 누락: {out}");
        assert!(out.contains("dumping users"), "로그 본문 누락");
        assert!(out.contains(r#"class="logdump""#), "로그 컨테이너 누락");
        assert!(out.contains("4242"), "pid 누락");
        assert!(out.contains("2 lines"), "로그 줄 수 누락");
    }

    /// 모르는 잡 id는 오류 블록을 보여주되 화면이 깨지지 않는다.
    #[test]
    fn unknown_job_renders_error_block() {
        let id = JobId::generate();
        let detail = JobDetail {
            id,
            summary: None,
            logs: Vec::new(),
            dropped_leading_logs: 0,
            log_file_present: false,
            unreadable_lines: 0,
        };
        let out = detail_body(Lang::En, &detail, &empty_registry()).into_string();
        assert!(
            out.contains(r#"data-level="error""#),
            "오류 레벨 누락: {out}"
        );
        assert!(out.contains("Unknown job"), "제목 누락");
        // 로그 파일 부재 설명도 함께 나온다.
        assert!(
            out.contains("removed by rotation"),
            "로테이션 설명 누락: {out}"
        );
    }

    /// 로테이션으로 로그가 사라진 잡은 요약을 보여주고 로그 부재를 설명한다.
    #[test]
    fn rotated_job_explains_missing_log() {
        let summary = finished(JobOutcome::Succeeded);
        let detail = JobDetail {
            id: summary.id,
            summary: Some(summary),
            logs: Vec::new(),
            dropped_leading_logs: 0,
            log_file_present: false,
            unreadable_lines: 0,
        };
        let out = detail_body(Lang::Ko, &detail, &empty_registry()).into_string();
        assert!(out.contains("로테이션"), "로그 부재 설명 누락: {out}");
        assert!(out.contains("backup"), "요약이 사라졌다");
    }

    /// 앞부분이 잘린 로그는 그 사실을 말한다.
    #[test]
    fn dropped_leading_logs_are_reported() {
        let summary = finished(JobOutcome::Succeeded);
        let detail = JobDetail {
            id: summary.id,
            summary: Some(summary),
            logs: vec![log("tail line")],
            dropped_leading_logs: 12,
            log_file_present: true,
            unreadable_lines: 0,
        };
        let out = detail_body(Lang::En, &detail, &empty_registry()).into_string();
        assert!(
            out.contains("Earlier lines omitted"),
            "생략 안내 누락: {out}"
        );
        assert!(out.contains("12"));
    }

    // -- 안전성 ----------------------------------------------------------

    /// 마크업에 색 리터럴·인라인 스타일이 없다([`components`] 계약 준수).
    #[test]
    fn markup_carries_no_presentation() {
        let summary = finished(JobOutcome::SucceededWithWarnings);
        let rendered = [
            list_body(
                Lang::En,
                &history_of(vec![finished(JobOutcome::Failed), running()]),
                &empty_registry(),
            )
            .into_string(),
            list_body(Lang::Ko, &history_of(Vec::new()), &empty_registry()).into_string(),
            detail_body(
                Lang::Ko,
                &detail_of(summary, vec![log("plain line")]),
                &empty_registry(),
            )
            .into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
        }
    }

    /// 적대적 로그(HTML 태그·제어문자·초장문)가 이스케이프·절단된다.
    #[test]
    fn hostile_log_content_is_escaped_and_truncated() {
        let hostile = r#"<script>alert(1)</script><img src=x onerror="alert(2)">"#;
        let very_long = "A".repeat(20_000);
        let control = "\u{0}\u{7}\u{1b}[31mred\u{1b}[0m";
        let summary = finished(JobOutcome::Failed);
        let detail = detail_of(summary, vec![log(hostile), log(&very_long), log(control)]);
        let out = detail_body(Lang::En, &detail, &empty_registry()).into_string();

        assert!(!out.contains("<script>"), "스크립트 태그가 살아 있다");
        // 따옴표가 `&quot;`로 접히므로 속성 경계가 살아남지 못한다(maud 자동 이스케이프).
        assert!(!out.contains("onerror=\""), "속성이 살아 있다");
        assert!(out.contains("&lt;script&gt;"), "이스케이프 형태가 아니다");
        // 초장문은 상한에서 잘린다.
        assert!(!out.contains(&very_long), "20,000자 줄이 그대로 실렸다");
        assert!(
            out.contains(&"A".repeat(MAX_TEXT_CHARS)),
            "상한까지는 남아 있어야 한다"
        );
        assert!(out.contains('…'), "절단 표시 누락");
    }

    /// 적대적 인자·프로파일 이름도 이스케이프된다(둘 다 남의 문자열이다).
    #[test]
    fn hostile_args_and_profile_are_escaped() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let mut summary = finished(JobOutcome::Failed);
        summary.profile = Some(hostile.to_string());
        summary.args_masked = vec![hostile.to_string()];
        let list = list_body(
            Lang::En,
            &history_of(vec![summary.clone()]),
            &empty_registry(),
        )
        .into_string();
        let detail =
            detail_body(Lang::En, &detail_of(summary, vec![]), &empty_registry()).into_string();
        for out in [list, detail] {
            assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
            assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다");
        }
    }

    /// 로그·인자에 섞인 등록된 시크릿이 화면에서 마스킹된다(2차 방어).
    #[test]
    fn registered_secrets_are_masked_in_logs_and_args() {
        const FAKE: &str = "NOT-A-REAL-SECRET-jobsview-7a1c93f2";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(FAKE), "표본 시크릿 등록 실패");

        let mut summary = finished(JobOutcome::Failed);
        summary.args_masked = vec![format!("--uri=postgres://u:{FAKE}@h/db")];
        let detail = detail_of(
            summary,
            vec![log(&format!("auth failed for password {FAKE}"))],
        );
        let out = detail_body(Lang::En, &detail, &registry).into_string();
        assert!(!out.contains(FAKE), "시크릿 원문이 화면에 남았다");
        assert_eq!(
            out.matches(crate::web::mask::REDACTED_PLACEHOLDER).count(),
            2,
            "인자와 로그 양쪽이 마스킹되어야 한다: {out}"
        );
    }

    /// 기간 표기가 단위별로 갈리고, 음수는 숨기지 않는다.
    #[test]
    fn duration_formatting_covers_each_unit() {
        use chrono::TimeDelta;
        assert_eq!(
            format_duration(Lang::En, TimeDelta::milliseconds(120)),
            "120ms"
        );
        assert_eq!(format_duration(Lang::En, TimeDelta::seconds(45)), "45s");
        assert_eq!(format_duration(Lang::En, TimeDelta::seconds(75)), "1m 15s");
        assert_eq!(
            format_duration(Lang::En, TimeDelta::seconds(3_780)),
            "1h 03m"
        );
        assert_eq!(format_duration(Lang::Ko, TimeDelta::seconds(45)), "45초");
        assert_eq!(
            format_duration(Lang::En, TimeDelta::seconds(-5)),
            "?",
            "음수 기간을 0으로 뭉개면 이상한 데이터가 숨는다"
        );
    }

    /// 시각은 UTC로 표기된다 — 자식 로그와 대조 가능해야 한다.
    #[test]
    fn timestamps_are_rendered_in_utc() {
        let rendered = format_ts(Utc.timestamp_opt(1_785_000_000, 0).unwrap());
        assert!(rendered.ends_with('Z'), "UTC 표시가 없다: {rendered}");
        assert_eq!(rendered, "2026-07-25 17:20:00Z");
    }
}
