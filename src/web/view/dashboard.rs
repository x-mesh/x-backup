//! 대시보드 마크업 — 프로파일 하나가 표의 한 행이다.
//!
//! ## 왜 카드가 아니라 표인가
//! `/doctor`는 프로파일마다 카드 한 장을 쓴다(항목이 프로파일마다 다르고 개수도 다르다).
//! 여기는 반대다: 보여주는 것이 프로파일마다 **정확히 같은 여섯 칸**이고
//! ([`route::CELL_KEYS`]와 프로브 시간), 운영자가 하는 일은 "어느 행이 다른가"를 찾는
//! 것이다. 그 일에는 표가 압도적으로 낫다 — 같은 열이 같은 x좌표에 있으면 눈이
//! 이상값을 스캔으로 찾고, 카드라면 매 카드마다 라벨을 다시 읽어야 한다.
//!
//! 그래서 열 위치를 **절대 흔들지 않는다.** 값이 없는 칸도 `—`로 남기고 지우지 않는다.
//! 엔진마다 없는 점검이 있어서(PostgreSQL에 secondary 개념이 없다) 칸을 지우기 시작하면
//! 행마다 열 수가 달라지고, 그 순간 표의 유일한 장점이 사라진다.
//!
//! ## 상태는 `data-level`로만 나간다
//! [`components::Level`]의 토큰 넷이 전부이고, 색·아이콘·테두리로 바꾸는 곳은
//! `app.css`의 `[data-level="…"]` 한 군데다(그 모듈 헤더의 계약). 이 파일에는 클래스
//! 이름과 `data-level`만 있고 색 리터럴·인라인 스타일이 없다 — 아래
//! `markup_carries_no_presentation` 테스트가 그것을 고정한다.
//!
//! `app.css`에 새 클래스를 추가하지 않았다. 이 브랜치에서 스타일시트는 여러 화면이
//! 공유하는 파일이고, 표에 필요한 것(`.dtable`·`.key`·`.num`·`.msg`·`.age`·`.badge`·
//! `.tag`·`.counts`)이 이미 전부 있다. 화면 하나를 붙이려고 공용 파일을 건드리는 것은
//! 배선 충돌을 만드는 값싼 방법이다.
//!
//! ## 근거(note)를 언제 보여주는가
//! 칸마다 자식이 만든 상세 메시지가 있지만, 전부 펼치면 행이 세 줄씩 되어 스캔이
//! 불가능해진다. 규칙은 둘이다:
//!
//! 1. **상태가 `ok`가 아닌 칸은 근거를 보여준다.** 운영자가 "왜?"를 물을 자리가 정확히
//!    거기다. 화면을 벗어나 CLI로 가게 만들지 않는다.
//! 2. **destination 칸은 항상 보여준다.** 여유 공간이 그 문장 안에 있기 때문이다
//!    (R11이 요구하는 값인데 `status --json`이 별도 필드로 주지 않는다 —
//!    [`route::cell_from`] doc 참조).
//!
//! 값과 근거가 같은 문자열이면(항목에 `value`가 없어서 메시지를 값으로 쓴 경우) 근거를
//! 생략한다 — 같은 문장을 두 번 읽히지 않는다.
//!
//! ## 함정
//! - 이 파일의 모든 자유 텍스트는 **자식 프로세스가 만든 남의 문자열**이다. maud의 자동
//!   이스케이프에 그대로 맡기고 [`maud::PreEscaped`]를 한 번도 쓰지 않는다 — 쓰는 순간
//!   저장형 XSS 경로가 열린다([`components`] 헤더의 같은 규율).
//! - 자유 텍스트는 전부 [`route::scrub`]을 지난다(레지스트리 마스킹 + URI 접기). 값을
//!   만드는 쪽이 이미 지웠더라도 화면이 마지막 방어선이다(`view::jobs` 헤더의 같은 판단).
//! - 서버 내부 정보(state 디렉터리·config 경로·실행 파일 경로)는 싣지 않는다. config는
//!   **존재 여부만** 말한다(`route::page`가 `bool`만 넘기는 이유).

use std::time::Duration;

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::mask::SecretRegistry;
use crate::web::routes::dashboard as route;
use crate::web::routes::dashboard::{Cell, Dashboard, ProfileRow, RowCells, RowState};
use crate::web::view::components::{self, Level};

/// 값이 없는 칸에 찍는 표식. 공백으로 두지 않는 이유는 "값이 없다"와 "렌더가 빠졌다"를
/// 구분해야 하기 때문이다(빈 셀은 버그처럼 보인다).
const ABSENT: &str = "—";

/// 화면 본문 — **순수 함수**(자식도 파일시스템도 시각도 건드리지 않는다).
///
/// `config_present`는 config 파일의 **존재 여부만** 받는다. 경로 자체는 서버 내부
/// 정보이므로 마크업에 싣지 않는다(모듈 헤더 "함정").
pub fn body(
    lang: Lang,
    config_present: bool,
    dashboard: &Dashboard,
    registry: &SecretRegistry,
) -> Markup {
    let subtitle = lang.sel(
        "Every profile on one screen — connection, topology, replication, last backup age, and destination headroom.",
        "전 프로파일을 한 화면에 — 연결·토폴로지·복제·마지막 백업 나이·destination 여유.",
    );
    html! {
        (components::page_head(route::DASHBOARD_TITLE, Some(subtitle)))
        @match dashboard {
            Dashboard::Rows { rows, roster_age, roster_hit, ttl, elapsed } => {
                (verdict(lang, dashboard, rows))
                (page_meta(lang, config_present, rows, *roster_age, *roster_hit, *ttl, *elapsed))
                @if rows.is_empty() {
                    (empty_roster(lang))
                } @else {
                    (rows_table(lang, rows, registry))
                }
                (refresh_note(lang, *ttl))
            }
            Dashboard::NoRoster { headline, detail, stderr } => {
                (components::verdict_banner(Level::Error, &route::scrub(registry, headline), Some(&route::scrub(registry, detail))))
                (no_roster_notice(lang, stderr, registry))
            }
        }
    }
}

/// 화면 최상단 한 줄 판정 — "지금 괜찮은가"에 대한 답.
fn verdict(lang: Lang, dashboard: &Dashboard, rows: &[ProfileRow]) -> Markup {
    let level = dashboard.level();
    let bad = dashboard.needs_attention();
    let total = rows.len();
    let (headline, detail) = if total == 0 {
        (
            lang.sel(
                "No profiles to watch.",
                "지켜볼 프로파일이 없습니다.",
            )
            .to_string(),
            lang.sel(
                "The console read the config but found no [profiles.<name>] section, so this screen has nothing to report.",
                "콘솔이 config를 읽었지만 [profiles.<name>] 섹션을 찾지 못했습니다 — 이 화면이 보고할 것이 없습니다.",
            )
            .to_string(),
        )
    } else if bad == 0 {
        (
            format!(
                "{total} {}",
                lang.sel("profiles checked — all clear.", "프로파일 점검 완료 — 모두 정상.")
            ),
            lang.sel(
                "Connection, topology, replication, last backup age and destination all report ok on every profile.",
                "모든 프로파일에서 연결·토폴로지·복제·마지막 백업 나이·destination이 정상입니다.",
            )
            .to_string(),
        )
    } else {
        (
            format!(
                "{bad}/{total} {}",
                lang.sel("profiles need attention.", "프로파일에 주의가 필요합니다.")
            ),
            lang.sel(
                "Rows are ordered by profile name, not by severity — read the badge column. A row marked ERROR means the check itself did not complete, which is different from a check that completed and said no.",
                "행은 심각도가 아니라 프로파일 이름순입니다 — 배지 열을 보세요. ERROR 행은 점검 자체가 끝나지 않았다는 뜻이고, 점검이 끝나고 '안 된다'고 답한 것과는 다릅니다.",
            )
            .to_string(),
        )
    };
    components::verdict_banner(level, &headline, Some(&detail))
}

/// 화면 상단 메타 — 규모·소요·데이터 나이.
///
/// **데이터 나이를 반드시 싣는다.** TTL 캐시가 있는 화면에서 "몇 초 전 값인지"를 적지
/// 않으면 화면이 조용히 거짓말을 한다([`crate::web::cache`] 헤더 "화면은 나이를 말한다").
#[allow(clippy::too_many_arguments)]
fn page_meta(
    lang: Lang,
    config_present: bool,
    rows: &[ProfileRow],
    roster_age: Duration,
    roster_hit: bool,
    ttl: Duration,
    elapsed: Duration,
) -> Markup {
    // 가장 낡은 값의 나이 — 화면 전체가 "최대 이만큼 낡았다"고 말하는 데 쓴다.
    let oldest = rows
        .iter()
        .map(|r| r.age)
        .chain(std::iter::once(roster_age))
        .max()
        .unwrap_or(Duration::ZERO);
    let live = rows.iter().filter(|r| !r.hit).count() + usize::from(!roster_hit);
    let config_text = if config_present {
        lang.sel("wired", "연결됨")
    } else {
        lang.sel("not wired", "연결 안 됨")
    };
    components::meta_list(&[
        ("profiles", rows.len().to_string()),
        (
            "attention",
            rows.iter()
                .filter(|r| r.level() != Level::Ok)
                .count()
                .to_string(),
        ),
        ("probe", fmt_millis(elapsed)),
        ("data age", fmt_age(lang, oldest)),
        ("probed now", live.to_string()),
        ("cache ttl", format!("{}s", ttl.as_secs())),
        ("config", config_text.to_string()),
    ])
}

/// 프로파일 표. 열 순서는 **절대 흔들지 않는다**(모듈 헤더).
fn rows_table(lang: Lang, rows: &[ProfileRow], registry: &SecretRegistry) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        // 라벨은 기술용어이므로 영문 고정([`crate::i18n`] 규약).
                        th scope="col" { "Status" }
                        th scope="col" { "Profile" }
                        th scope="col" { "Engine" }
                        th scope="col" { "Connection" }
                        th scope="col" { "Topology" }
                        th scope="col" { "Replication" }
                        th scope="col" { "Last backup" }
                        th scope="col" { "Destination" }
                        th scope="col" { "Probe" }
                    }
                }
                tbody {
                    @for row in rows {
                        (table_row(lang, row, registry))
                    }
                }
            }
        }
    }
}

/// 행 하나. `<tr>`에도 `data-level`을 실어 CSS가 행 전체를 물들일 수 있게 한다 —
/// 배지 하나만으로는 행이 스무 개를 넘어가면 눈이 상태를 잃는다.
fn table_row(lang: Lang, row: &ProfileRow, registry: &SecretRegistry) -> Markup {
    let level = row.level();
    html! {
        tr data-level=(level.token()) {
            td { (status_cell(lang, row, registry)) }
            // `key` = 이 행의 식별 열. CSS가 최소 폭을 걸어 여러 행의 다음 열이 같은
            // x에서 시작하게 한다(app.css `.dtable .key`).
            //
            // 프로파일 이름과 엔진 라벨도 [`route::scrub`]을 지난다. 우리가 지은 이름이
            // 아니라 **config에 사람이 적은 문자열**이고(명부는 `doctor`가 그것을 그대로
            // 돌려준다), 제어문자가 섞인 이름이 화면·복사한 로그로 새는 경로를 남길 이유가
            // 없다. 정상 이름에는 아무 영향이 없다(scrub은 깨끗한 텍스트에 무연산이다).
            td class="mono key" { (route::scrub(registry, &row.profile)) }
            td {
                @match &row.engine {
                    Some(engine) => span class="tag" { (route::scrub(registry, engine)) },
                    // `doctor`가 URI를 해석하지 못한 프로파일이다 — 오류가 아니다
                    // (env 미설정·인라인 오타 등은 `/doctor`가 설명한다).
                    None => span class="muted" { (lang.sel("unknown", "미확인")) },
                }
            }
            @match &row.state {
                RowState::Reported { cells, .. } => (cell_columns(lang, cells, registry)),
                // 진단은 남은 열을 통째로 쓴다 — 빈 칸 다섯 개를 늘어놓으면 "값이 없다"와
                // "점검을 못 했다"가 구분되지 않는다. 행 수는 그대로 하나다.
                RowState::Unreadable { exit, headline, detail, parse, stderr } => {
                    td class="msg" colspan="5" {
                        (unreadable_cell(lang, exit, headline, detail, parse, stderr, registry))
                    }
                }
                RowState::Unavailable { headline, detail } => {
                    td class="msg" colspan="5" {
                        p { (route::scrub(registry, headline)) }
                        p class="muted" { (route::scrub(registry, detail)) }
                    }
                }
            }
            td class="num" { (probe_cell(lang, row)) }
        }
    }
}

/// 상태 칸 — 배지 + 종료 코드 사실(+ 자식 신호등이 어긋날 때만 그 값).
fn status_cell(lang: Lang, row: &ProfileRow, registry: &SecretRegistry) -> Markup {
    let level = row.level();
    html! {
        (components::badge(level))
        @match &row.state {
            RowState::Reported { exit, overall, counts, level: row_level, .. } => {
                // 종료 코드를 글자로 남긴다 — 배지 색만으로는 "왜 노랑인가"를 알 수 없고,
                // 운영자가 같은 명령을 손으로 재현할 때 무엇이 나올지도 알 수 있다.
                div class="muted" { (exit.text()) }
                div class="counts" {
                    (format!("{} ok · {} warn · {} fail", counts[0], counts[1], counts[2]))
                }
                // 자식이 스스로 매긴 신호등이 우리 판정과 어긋나면 그 사실을 드러낸다.
                // 정상 CLI에서는 일어날 수 없으므로(둘이 같은 항목에서 계산된다), 보이면
                // 콘솔과 CLI가 다른 빌드라는 신호다.
                @if overall != row_level.token() {
                    // `overall`은 자식이 만든 문자열이다 — 다른 자유 텍스트와 같은
                    // 위생 함수를 지난다.
                    div { span class="tag" { "overall " (route::scrub(registry, overall)) } }
                }
            }
            // 진단 행은 종료 코드를 진단 칸에서 자세히 설명하므로 여기서는 배지만 남긴다.
            RowState::Unreadable { .. } | RowState::Unavailable { .. } => {
                div class="muted" { (lang.sel("no result", "결과 없음")) }
            }
        }
    }
}

/// 다섯 개의 값 칸 — 순서가 표 머리글과 1:1이다.
fn cell_columns(lang: Lang, cells: &RowCells, registry: &SecretRegistry) -> Markup {
    html! {
        (value_cell(lang, cells.connection.as_ref(), false, registry))
        (value_cell(lang, cells.topology.as_ref(), false, registry))
        (value_cell(lang, cells.replication.as_ref(), false, registry))
        // 마지막 백업은 나이(경과 시간)이므로 `.age`로 표기를 고정한다(tabular-nums).
        (age_cell(lang, cells.last_backup.as_ref(), registry))
        // destination은 근거를 **항상** 보여준다 — 여유 공간이 그 문장 안에 있다
        // (모듈 헤더 "근거를 언제 보여주는가" 규칙 2).
        (value_cell(lang, cells.destination.as_ref(), true, registry))
    }
}

/// 값 칸 하나. `always_note`가 참이면 상태가 `ok`여도 근거를 함께 보여준다.
fn value_cell(
    lang: Lang,
    cell: Option<&Cell>,
    always_note: bool,
    registry: &SecretRegistry,
) -> Markup {
    match cell {
        // 이 엔진에 없는 점검이다 — 오류가 아니므로 `data-level`을 붙이지 않는다
        // (붙이면 CSS가 색을 입혀 "상태가 있다"고 말하게 된다).
        None => html! { td class="muted" { (ABSENT) } },
        Some(cell) => html! {
            td data-level=(cell.level.token()) {
                (route::scrub(registry, &cell.text))
                (note(lang, cell, always_note, registry))
            }
        },
    }
}

/// 마지막 백업 칸 — 값이 경과 시간이므로 `.age` 표기를 쓴다.
fn age_cell(lang: Lang, cell: Option<&Cell>, registry: &SecretRegistry) -> Markup {
    match cell {
        None => html! { td class="muted" { (ABSENT) } },
        Some(cell) => html! {
            td class="age" data-level=(cell.level.token()) {
                (route::scrub(registry, &cell.text))
                (note(lang, cell, false, registry))
            }
        },
    }
}

/// 칸의 근거 줄 — 보여줄 조건은 모듈 헤더 "근거를 언제 보여주는가" 참조.
fn note(lang: Lang, cell: &Cell, always: bool, registry: &SecretRegistry) -> Markup {
    let _ = lang;
    let show = always || cell.level != Level::Ok;
    // 값과 근거가 같으면(항목에 `value`가 없었다) 같은 문장을 두 번 읽히지 않는다.
    let redundant = cell.note.trim() == cell.text.trim();
    html! {
        @if show && !redundant && !cell.note.is_empty() {
            div class="muted" { (route::scrub(registry, &cell.note)) }
        }
    }
}

/// 프로브 칸 — 실측 소요 + 이 값의 출처(실측/캐시).
fn probe_cell(lang: Lang, row: &ProfileRow) -> Markup {
    html! {
        (fmt_millis(row.elapsed))
        div class="muted" {
            @if row.hit {
                (lang.sel("cached ", "캐시 ")) (fmt_age(lang, row.age))
            } @else {
                (lang.sel("live", "실측"))
            }
        }
    }
}

/// stdout을 읽지 못한 행의 진단 칸.
///
/// 종료 코드 설명과 파싱 실패 이유를 **함께** 싣는다. 파싱 실패의 원인은 대개 "자식이
/// JSON을 찍기 전에 다른 이유로 끝났다"이고, 그 이유는 종료 코드에 남는다(config 미연결
/// 이면 exit 2, 대상 점검 실패면 exit 3).
#[allow(clippy::too_many_arguments)]
fn unreadable_cell(
    lang: Lang,
    exit: &route::ExitFacts,
    headline: &str,
    detail: &str,
    parse: &str,
    stderr: &str,
    registry: &SecretRegistry,
) -> Markup {
    let masked_stderr = route::scrub(registry, stderr);
    html! {
        p { (route::scrub(registry, headline)) }
        p class="muted" { (route::scrub(registry, detail)) }
        p class="muted" { (route::scrub(registry, parse)) }
        div class="counts" { (exit.text()) }
        @if masked_stderr.is_empty() {
            p class="muted" {
                (lang.sel(
                    "The child wrote nothing to stderr either.",
                    "자식이 stderr에도 아무것도 쓰지 않았습니다.",
                ))
            }
        } @else {
            pre class="logdump" { (masked_stderr) }
        }
    }
}

/// 프로파일이 0개인 config의 안내.
fn empty_roster(lang: Lang) -> Markup {
    components::notice(
        Level::Warn,
        // 영문 라벨 고정 — 아래 문장만 언어를 탄다.
        "No profiles",
        html! {
            p {
                (lang.sel(
                    "Add a [profiles.<name>] section to the config the console was started with, then reload.",
                    "콘솔이 함께 기동된 config에 [profiles.<name>] 섹션을 추가한 뒤 새로고침하세요.",
                ))
            }
        },
    )
}

/// 명부 자체를 못 얻었을 때의 진단 블록.
fn no_roster_notice(lang: Lang, stderr: &str, registry: &SecretRegistry) -> Markup {
    let masked = route::scrub(registry, stderr);
    components::notice(
        Level::Error,
        lang.sel("What to check", "확인할 것"),
        html! {
            p {
                (lang.sel(
                    "This screen asks the console's own binary for the profile list before it checks anything. Without that list there is nothing to draw — not even a partial page.",
                    "이 화면은 무엇을 점검하기 전에 콘솔 자신의 바이너리에게 프로파일 목록을 먼저 묻습니다. 그 목록이 없으면 부분 화면조차 그릴 수 없습니다.",
                ))
            }
            p {
                (lang.sel(
                    "Start the console with --config <PATH> (or XB_CONFIG) so it has something to read, and check that its binary is still readable and executable.",
                    "콘솔을 --config <PATH>(또는 XB_CONFIG)와 함께 기동해 읽을 대상을 주고, 바이너리가 여전히 읽기·실행 가능한지 확인하세요.",
                ))
            }
            @if !masked.is_empty() {
                pre class="logdump" { (masked) }
            }
        },
    )
}

/// 새로고침 안내 — 이 화면이 스스로 갱신하지 않는다는 사실을 **화면에 적는다.**
///
/// 자동 갱신을 넣지 않은 것은 의도된 선택이고(그 근거는 [`route`] 모듈 헤더), 의도된
/// 선택이라도 사용자가 모르면 결함처럼 느껴진다. "이 값은 언제 바뀌는가"를 화면이
/// 대답하면 운영자는 새로고침을 언제 눌러야 하는지 안다.
fn refresh_note(lang: Lang, ttl: Duration) -> Markup {
    html! {
        p class="muted" {
            (lang.sel(
                "This screen does not refresh itself — reload to re-check. ",
                "이 화면은 스스로 갱신하지 않습니다 — 다시 점검하려면 새로고침하세요. ",
            ))
            (format!(
                "{}{}s{}",
                lang.sel("Results are reused for ", "결과는 "),
                ttl.as_secs(),
                lang.sel(", so reloading sooner shows the same numbers without touching the databases again.", "초 동안 재사용되므로, 그 안에 새로고침하면 DB를 다시 건드리지 않고 같은 값을 보여줍니다."),
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// 시간 표기
// ---------------------------------------------------------------------------

/// 소요 시간 표기 — 1초 미만은 ms, 그 이상은 초.
///
/// 단위를 섞는 이유: 정상 프로브는 수십~수백 ms라 초로 적으면 `0.14 s`처럼 앞자리가
/// 전부 0이 되어 비교가 안 된다. 반대로 3초짜리를 `3120 ms`로 적으면 자릿수를 세야 한다.
/// 사람이 크기를 즉시 읽을 수 있는 쪽을 고른다.
fn fmt_millis(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("{ms} ms")
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

/// 나이 표기 — 초/분 단위. 라벨이 아니라 설명이므로 언어를 탄다.
fn fmt_age(lang: Lang, d: Duration) -> String {
    let secs = d.as_secs();
    if d < Duration::from_millis(500) {
        return lang.sel("just now", "방금").to_string();
    }
    if secs < 60 {
        return format!("{secs}s");
    }
    format!("{}m {}s", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::cache::PROBE_TTL;
    use crate::web::job::JobOutcome;
    use crate::web::routes::dashboard::ExitFacts;

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    /// 값 칸 하나짜리 셀 표본.
    fn cell(level: Level, text: &str, note: &str) -> Cell {
        Cell {
            level,
            text: text.to_string(),
            note: note.to_string(),
        }
    }

    /// 정상 보고 행 하나.
    fn reported_row(profile: &str, level: Level) -> ProfileRow {
        ProfileRow {
            profile: profile.to_string(),
            engine: Some("mongodb".to_string()),
            state: RowState::Reported {
                level,
                overall: level.token().to_string(),
                exit: ExitFacts::of(&JobOutcome::Succeeded),
                counts: [5, 0, 0, 0],
                cells: RowCells {
                    connection: Some(cell(Level::Ok, "SCRAM-SHA-256", "connected")),
                    topology: Some(cell(Level::Ok, "replica set", "rs0 (3 members)")),
                    replication: None,
                    last_backup: Some(cell(Level::Ok, "3h ago", "full, 1.2 GiB, 3h ago")),
                    destination: Some(cell(
                        Level::Ok,
                        "OK",
                        "writable (local: /srv/backups), free 12.3 GiB",
                    )),
                },
            },
            elapsed: Duration::from_millis(142),
            age: Duration::ZERO,
            hit: false,
        }
    }

    fn dashboard_of(rows: Vec<ProfileRow>) -> Dashboard {
        Dashboard::Rows {
            rows,
            roster_age: Duration::ZERO,
            roster_hit: false,
            ttl: PROBE_TTL,
            elapsed: Duration::from_millis(210),
        }
    }

    /// 열 개수가 머리글과 본문에서 일치한다 — 어긋나면 표가 통째로 밀린다.
    #[test]
    fn header_and_body_column_counts_agree() {
        let dashboard = dashboard_of(vec![reported_row("prod", Level::Ok)]);
        let html = body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        let headers = html.matches("<th scope=\"col\">").count();
        assert_eq!(headers, 9, "머리글 열 수가 바뀌었다");
        // 본문 행은 상태·프로파일·엔진 + 값 5칸 + 프로브 = 9칸.
        let body_start = html.find("<tbody>").expect("tbody 없음");
        let row_html = &html[body_start..];
        assert_eq!(
            row_html.matches("<td").count(),
            9,
            "본문 칸 수가 머리글과 다르다: {row_html}"
        );
    }

    /// 값이 없는 칸도 지우지 않는다 — 열 위치가 흔들리면 표의 장점이 사라진다.
    #[test]
    fn absent_cells_keep_their_column() {
        let dashboard = dashboard_of(vec![reported_row("prod", Level::Ok)]);
        let html = body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(html.contains(ABSENT), "빈 칸 표식이 없다");
        // 빈 칸에는 `data-level`을 붙이지 않는다(상태가 없으므로 색도 없다).
        assert!(
            html.contains(&format!(r#"<td class="muted">{ABSENT}</td>"#)),
            "빈 칸이 상태를 가진 것처럼 렌더됐다: {html}"
        );
    }

    /// `ok` 칸은 근거를 접고, `ok`가 아닌 칸과 destination은 펼친다.
    #[test]
    fn notes_appear_only_where_they_help() {
        let mut row = reported_row("prod", Level::Warn);
        if let RowState::Reported { cells, .. } = &mut row.state {
            cells.connection = Some(cell(
                Level::Warn,
                "SCRAM-SHA-1",
                "weak mechanism in use — rotate the credential",
            ));
        }
        let html = body(Lang::En, true, &dashboard_of(vec![row]), &empty_registry()).into_string();
        // 경고 칸의 근거는 보인다.
        assert!(html.contains("rotate the credential"), "경고 근거가 접혔다");
        // destination 근거(여유 공간)는 ok여도 보인다.
        assert!(html.contains("free 12.3 GiB"), "여유 공간이 접혔다");
        // ok인 topology 근거는 접힌다.
        assert!(!html.contains("rs0 (3 members)"), "ok 칸의 근거가 펼쳐졌다");
    }

    /// 값과 근거가 같으면 같은 문장을 두 번 내지 않는다.
    #[test]
    fn identical_value_and_note_render_once() {
        let mut row = reported_row("prod", Level::Fail);
        if let RowState::Reported { cells, .. } = &mut row.state {
            cells.connection = Some(cell(
                Level::Fail,
                "connection refused",
                "connection refused",
            ));
        }
        let html = body(Lang::En, true, &dashboard_of(vec![row]), &empty_registry()).into_string();
        assert_eq!(
            html.matches("connection refused").count(),
            1,
            "같은 문장이 두 번 나왔다: {html}"
        );
    }

    /// 진단 행도 `<tr>` 하나다 — "프로파일마다 한 행" 규약.
    #[test]
    fn diagnostic_rows_are_still_one_row_each() {
        let rows = vec![
            reported_row("a", Level::Ok),
            ProfileRow {
                profile: "b".into(),
                engine: None,
                state: RowState::Unavailable {
                    headline: "timed out".into(),
                    detail: "suspect the target".into(),
                },
                elapsed: Duration::from_secs(8),
                age: Duration::ZERO,
                hit: false,
            },
            ProfileRow {
                profile: "c".into(),
                engine: Some("mysql".into()),
                state: RowState::Unreadable {
                    exit: ExitFacts::of(&JobOutcome::Rejected),
                    headline: "console sent a bad request".into(),
                    detail: "exit 2 — usage error".into(),
                    parse: "no json".into(),
                    stderr: "boom".into(),
                },
                elapsed: Duration::from_millis(9),
                age: Duration::from_secs(4),
                hit: true,
            },
        ];
        let html = body(Lang::En, true, &dashboard_of(rows), &empty_registry()).into_string();
        let body_start = html.find("<tbody>").expect("tbody 없음");
        assert_eq!(
            html[body_start..].matches("<tr").count(),
            3,
            "행 수가 프로파일 수와 다르다"
        );
        assert!(html.contains("timed out"), "진단 문장 누락");
        assert!(html.contains("exit 2 · rejected"), "종료 코드 표기 누락");
        assert!(html.contains("cached 4s"), "캐시 나이 표기 누락");
        assert!(html.contains("live"), "실측 표기 누락");
    }

    /// 데이터 나이가 화면에 적힌다 — TTL 캐시가 있는 화면의 필수 표기.
    #[test]
    fn page_states_the_age_of_its_data() {
        let mut row = reported_row("prod", Level::Ok);
        row.age = Duration::from_secs(9);
        row.hit = true;
        let dashboard = Dashboard::Rows {
            rows: vec![row],
            roster_age: Duration::from_secs(12),
            roster_hit: true,
            ttl: PROBE_TTL,
            elapsed: Duration::from_millis(3),
        };
        let html = body(Lang::En, true, &dashboard, &empty_registry()).into_string();
        assert!(html.contains("data age"), "나이 라벨 누락");
        // 가장 낡은 값(명부 12초)을 말한다.
        assert!(html.contains("12s"), "가장 낡은 나이가 아니다: {html}");
        assert!(html.contains("cache ttl"), "TTL 표기 누락");
        assert!(
            html.contains("does not refresh itself"),
            "자동 갱신 없음이 화면에 적혀 있지 않다"
        );
    }

    /// 자식 신호등이 우리 판정과 어긋나면 그 사실이 드러난다.
    #[test]
    fn disagreeing_overall_is_surfaced() {
        let mut row = reported_row("prod", Level::Fail);
        if let RowState::Reported { overall, .. } = &mut row.state {
            *overall = "ok".to_string();
        }
        let html = body(Lang::En, true, &dashboard_of(vec![row]), &empty_registry()).into_string();
        assert!(
            html.contains("overall ok"),
            "어긋남이 드러나지 않았다: {html}"
        );

        // 일치하면 굳이 표시하지 않는다(잡음이 된다).
        let html = body(
            Lang::En,
            true,
            &dashboard_of(vec![reported_row("prod", Level::Ok)]),
            &empty_registry(),
        )
        .into_string();
        assert!(!html.contains("overall ok"), "일치하는데 표시됐다");
    }

    /// 적대적 문자열(제어문자·HTML 태그·초대형)이 전부 이스케이프되고 렌더가 죽지 않는다.
    #[test]
    fn hostile_strings_are_escaped_and_do_not_break_rendering() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let huge = "A".repeat(200_000);
        let control = "line1\u{0}\u{7}\u{1b}[31mred\u{1b}[0m";
        let mut row = reported_row(&format!("{hostile}{control}"), Level::Fail);
        if let RowState::Reported { cells, overall, .. } = &mut row.state {
            *overall = hostile.to_string();
            cells.connection = Some(cell(Level::Fail, hostile, &huge));
            cells.destination = Some(cell(Level::Warn, control, hostile));
        }
        let rows = vec![
            row,
            ProfileRow {
                profile: hostile.to_string(),
                engine: Some(hostile.to_string()),
                state: RowState::Unreadable {
                    exit: ExitFacts::of(&JobOutcome::Unknown),
                    headline: hostile.to_string(),
                    detail: control.to_string(),
                    parse: huge.clone(),
                    stderr: hostile.to_string(),
                },
                elapsed: Duration::from_millis(1),
                age: Duration::ZERO,
                hit: false,
            },
        ];
        let html = body(Lang::En, true, &dashboard_of(rows), &empty_registry()).into_string();
        assert!(!html.contains("<img"), "이스케이프되지 않았다");
        assert!(!html.contains("onerror=\""), "속성이 살아 있다");
        assert!(html.contains("&lt;img"), "이스케이프 형태가 아니다");
        assert!(!html.contains('\u{0}'), "널 바이트가 그대로 나갔다");
    }

    /// 명부 실패 화면도 배너 + 안내로 렌더되고 stderr가 비어도 죽지 않는다.
    #[test]
    fn no_roster_page_renders_with_and_without_stderr() {
        for stderr in ["", "config not found"] {
            let dashboard = Dashboard::NoRoster {
                headline: "could not list profiles".into(),
                detail: "exit 2 — no config".into(),
                stderr: stderr.into(),
            };
            let html = body(Lang::En, true, &dashboard, &empty_registry()).into_string();
            assert!(html.contains("could not list profiles"), "배너 누락");
            assert!(html.contains("What to check"), "안내 블록 누락");
            assert_eq!(
                html.contains("logdump"),
                !stderr.is_empty(),
                "stderr 유무에 따른 덤프 처리가 잘못됐다"
            );
        }
    }

    /// 마크업에 색 리터럴·인라인 스타일이 없다 — 모양은 전부 CSS 몫이라는 계약.
    #[test]
    fn markup_carries_no_presentation() {
        let rows = vec![
            reported_row("a", Level::Ok),
            reported_row("b", Level::Warn),
            ProfileRow {
                profile: "c".into(),
                engine: None,
                state: RowState::Unavailable {
                    headline: "not checked".into(),
                    detail: "budget".into(),
                },
                elapsed: Duration::ZERO,
                age: Duration::ZERO,
                hit: false,
            },
        ];
        for dashboard in [
            dashboard_of(rows),
            dashboard_of(Vec::new()),
            Dashboard::NoRoster {
                headline: "nope".into(),
                detail: "why".into(),
                stderr: "boom".into(),
            },
        ] {
            let html = body(Lang::En, true, &dashboard, &empty_registry()).into_string();
            assert!(!html.contains("style="), "인라인 스타일이 들어갔다: {html}");
            assert!(!html.contains('#'), "색 리터럴로 보이는 값이 있다: {html}");
            assert!(
                html.contains("data-level="),
                "상태가 data-level로 나가지 않는다"
            );
        }
    }

    /// 두 언어 모두 렌더되고, 기술 라벨은 영문 그대로 남는다(R42).
    #[test]
    fn both_languages_render_and_labels_stay_english() {
        for lang in [Lang::En, Lang::Ko] {
            let html = body(
                lang,
                true,
                &dashboard_of(vec![reported_row("prod", Level::Ok)]),
                &empty_registry(),
            )
            .into_string();
            for label in [
                "Status",
                "Profile",
                "Engine",
                "Connection",
                "Topology",
                "Replication",
                "Last backup",
                "Destination",
                "Probe",
            ] {
                assert!(
                    html.contains(label),
                    "{lang:?}에서 라벨이 번역됐다: {label}"
                );
            }
        }
        // 설명 문장은 언어를 탄다.
        let ko = body(
            Lang::Ko,
            true,
            &dashboard_of(vec![reported_row("prod", Level::Ok)]),
            &empty_registry(),
        )
        .into_string();
        assert!(ko.contains("프로파일"), "ko 설명 문장이 없다");
    }

    /// 시간 표기 — 단위가 크기에 따라 바뀌고 자릿수가 폭주하지 않는다.
    #[test]
    fn duration_formatting_switches_units() {
        assert_eq!(fmt_millis(Duration::from_millis(0)), "0 ms");
        assert_eq!(fmt_millis(Duration::from_millis(142)), "142 ms");
        assert_eq!(fmt_millis(Duration::from_millis(999)), "999 ms");
        assert_eq!(fmt_millis(Duration::from_millis(1000)), "1.00 s");
        assert_eq!(fmt_millis(Duration::from_millis(8250)), "8.25 s");

        assert_eq!(fmt_age(Lang::En, Duration::from_millis(100)), "just now");
        assert_eq!(fmt_age(Lang::Ko, Duration::from_millis(100)), "방금");
        assert_eq!(fmt_age(Lang::En, Duration::from_secs(9)), "9s");
        assert_eq!(fmt_age(Lang::En, Duration::from_secs(125)), "2m 5s");
    }
}
