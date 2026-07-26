//! 백업 실행 화면(`GET/POST /backup`)과 실행 중 화면(`GET /backup/{id}`)의 마크업.
//!
//! ## 이 파일이 하는 일 / 하지 않는 일
//! [`crate::web::routes`] 모듈 헤더의 규약대로 이 파일은 **순수 렌더 함수**만 담는다 —
//! 자식 프로세스도, 파일시스템도, 잡 레지스트리도 직접 만지지 않는다. 잡을 spawn하고
//! 배선하는 일은 전부 [`crate::web::routes::backup`]의 몫이고, 이 파일은 그 결과(프로파일
//! 목록·[`JobDetail`]·경로 문자열)만 받아 마크업으로 접는다.
//!
//! ## 새 CSS 클래스를 만들지 않는다
//! [`components`] 헤더의 계약을 그대로 따른다. 폼은 `view/config.rs`가 이미 쓰고 있는
//! `.field`(`app.css`에 정의됨)를 재사용하고, 버튼 묶음은 `.actions`를 쓴다. 상태는
//! `data-level` 토큰으로만 말한다(색·인라인 스타일 리터럴 없음) — 아래
//! `markup_carries_no_presentation` 테스트가 고정한다.
//!
//! ## 실행 중 화면의 라이브 갱신 — 왜 인라인 `<script>`인가
//! [`crate::web::view`] 모듈 헤더는 "htmx 스크립트 태그는 아직 넣지 않는다 ... 실제로
//! 부분 갱신을 쓰는 화면(t15~)이 생길 때 벤더 JS를 함께 임베드해야 한다"고 이 태스크를
//! 정확히 지목해 두었다. 하지만 새 벤더 자산을 임베드하려면 `view/mod.rs`에 `include_str!`
//! 상수를 추가해야 하는데, 그 파일은 "모듈 선언 블록 맨 끝에 `pub mod backup;` 한 줄만"
//! 허용된 소유권 경계 밖이다. 그래서 이 화면은 htmx 없이 **표준 `EventSource`를 쓰는
//! 순정(vanilla) JS**를 이 파일 안의 `<script>` 하나로 완결한다 — 새 파일도, 새 의존성도,
//! `view/mod.rs` 추가 변경도 없다.
//!
//! [`RUNNING_SCRIPT`]는 **컴파일 타임 상수**이고 사용자 입력을 단 한 글자도 보간하지
//! 않는다 — [`components`] 헤더의 "`PreEscaped`는 컴파일 타임 상수에만" 규약을 지키는
//! 유일한 방법이 이것이다. 잡 id·SSE 경로 같은 동적 값은 `data-*` 속성으로 건너와(maud의
//! 자동 이스케이프를 그대로 통과한다) 스크립트가 런타임에 DOM에서 읽는다 — 문자열
//! 보간으로 스크립트 본문을 조립하면 그 규약이 깨진다.
//!
//! ## 상태 배지의 레벨은 **서버가 정한다** — 스크립트는 반영만 한다
//! 이 화면은 같은 상태를 두 경로로 그린다: 요청 시점의 서버 렌더(초기 HTML)와 그 뒤
//! [`RUNNING_SCRIPT`]가 받는 SSE 갱신. 한동안 **두 경로가 각자 레벨을 정했고**, 둘 다
//! 틀렸다 — 서버는 `is_running ? "ok" : "warn"`으로, 스크립트는 종료 시 무조건 `"warn"`을
//! 박았다. 결과가 어땠는지 보면 심각성이 분명하다: **exit 0 성공이 경고 배지로 떴고**,
//! succeeded / failed / rejected / precheck-failed / signaled가 라이브 화면에서 배지로 전혀
//! 구분되지 않았다. `summary.outcome`이 이미 손에 있는데 쓰지 않은 것이다.
//!
//! 지금은 레벨의 출처가 하나다 — [`job_exit::level_for_label`](crate::web::job::exit::level_for_label)
//! (t13, 잡 이력 화면과 같은 함수). 서버 렌더는 [`state_badge`]로 그 값을 쓰고, SSE 이벤트는
//! [`crate::web::sse::JobEvent`]에 **서버가 계산한 레벨 토큰을 실어** 보내고, 스크립트는 그
//! 값을 `data-level`에 그대로 넣는다. 레벨 매핑을 JS에도 두면 Rust와 JS 두 언어로 갈라져
//! 다시 어긋난다 — 아래 `script_does_not_decide_levels_itself` 테스트가 스크립트에 레벨
//! 리터럴이 없음을 고정한다.
//!
//! ## 남의 텍스트를 다루는 규칙(잡 이력 화면과 동일)
//! 로그 줄·인자는 자식 프로세스가 만든 문자열이다. [`crate::web::view::jobs`]와 같은
//! 두 규칙을 따른다: (1) [`SecretRegistry::mask`]를 무조건 한 번 더 통과시킨다(2차
//! 방어), (2) [`MAX_LOG_LINE_CHARS`]로 문자 경계에서 자른다. 이스케이프는 maud의 자동
//! 처리에 맡긴다 — 이 파일에 `PreEscaped`는 [`RUNNING_SCRIPT`] 하나뿐이다.

use maud::{html, Markup, PreEscaped};

use crate::i18n::Lang;
use crate::web::job::exit as job_exit;
use crate::web::mask::SecretRegistry;
use crate::web::routes::backup::{
    backup_cancel_href, backup_events_href, BACKUP_PATH, BACKUP_TITLE,
};
use crate::web::routes::jobs::job_detail_href;
use crate::web::state::jobs::{JobDetail, JobId, JobLogLine};
use crate::web::view::components::{self, Level};

/// 실행 중 화면에서 로그 한 줄을 그릴 때의 문자 수 상한.
///
/// [`crate::web::view::jobs::MAX_TEXT_CHARS`]와 같은 값·같은 근거이지만 독립된 상수로
/// 둔다 — 이 화면은 라이브(SSE) 로그를, 그 화면은 사후(파일) 로그를 그리므로 성격이
/// 다르고, 상수를 공유하면 한쪽을 조정할 때 다른 화면까지 흔들린다.
pub const MAX_LOG_LINE_CHARS: usize = 2_000;

/// 아직 도는 잡의 상태 배지 텍스트. 기술용어이므로 영문 고정([`crate::i18n`] 규약)이고,
/// SSE `state` 이벤트가 실어 보내는 어휘와 같은 값이다
/// ([`crate::web::routes::backup::spawn_tracked_job`]이 `"running"`을 publish한다).
pub const RUNNING_STATE_LABEL: &str = "running";

/// 취소 요청을 처리하는 동안의 상태 배지 텍스트.
///
/// [`crate::web::routes::backup::cancel`]이 SSE `state` 이벤트로 발행한다 —
/// [`crate::web::sse::JobEvent::State`] doc이 예시로 들던 어휘이지만 발행하는 곳이 없어서,
/// 운영자는 취소 버튼을 누른 뒤 최대 ~32초 동안 `running` 배지와 멈춘 POST만 봤다.
///
/// **이 상태는 서버 렌더에는 나타나지 않는다.** 취소 진행 여부는 어디에도 영속되지 않으므로
/// (이력에는 시작·종료 두 사건만 있다) 새로고침하면 다시 `running`으로 보인다. 취소 중임을
/// 영속하려면 잡 이력에 상태 전이 레코드를 추가해야 하는데 그건 저장 스키마 변경이라 이
/// 결함의 범위를 넘는다 — 라이브 화면에서 즉시 보이는 것만으로 원래 문제("눌렀는데 아무
/// 반응이 없다")는 해소된다.
pub const CANCELLING_STATE_LABEL: &str = "cancelling";

/// 끝났는데 결과 라벨이 없는(또는 요약 자체가 없는) 잡의 배지 텍스트.
///
/// [`crate::web::job::JobOutcome::Unknown`]의 라벨과 **같은 문자열**을 쓴다 — 그래야
/// [`job_exit::level_for_label`]이 이 값을 아는 어휘로 받아 [`Level::Error`]("판정 불가")를
/// 준다. 예전처럼 `"done"`을 쓰면 그 문자열은 결과 어휘에 없어서 모르는 라벨로 떨어지는데,
/// 화면 글자는 "끝났다"고 말하면서 색은 "모른다"가 되어 서로 다른 이야기를 한다.
/// 아래 `unknown_state_label_matches_the_outcome_vocabulary` 테스트가 두 값을 묶어 둔다.
pub const UNKNOWN_STATE_LABEL: &str = "unknown";

/// 실행 화면의 라이브 갱신 스크립트 — 컴파일 타임 상수(모듈 헤더 참조).
///
/// `#backup-run`의 `data-events-href` 속성에서 SSE 경로를 읽어 `EventSource`를 연다.
/// [`crate::web::sse::JobEvent`]의 네 종류(`progress`/`log`/`state`/`done`)를 각각
/// `addEventListener`로 받는다 — SSE 프레임의 `event:` 필드 이름과
/// [`crate::web::sse::JobEvent::kind`]가 같은 어휘를 쓴다는 그 모듈의 계약을 그대로
/// 활용한다. DOM 갱신은 전부 `textContent`/속성 값만 쓴다(`innerHTML` 없음) — 자식이
/// 낸 로그 줄이 그대로 여기 도달하므로, `innerHTML`을 썼다면 서버 측 이스케이프가
/// 전혀 없는 이 경로가 곧 저장형 XSS가 됐을 것이다. 색은 절대 설정하지 않는다 —
/// `data-level` 속성만 바꾸고, 그 값을 색으로 바꾸는 것은 언제나 `app.css` 하나뿐이다.
///
/// **레벨 자체도 이 스크립트가 정하지 않는다** — `state`/`done` 이벤트가 실어 온
/// `data.level`(서버가 [`job_exit::level_for_label`]로 계산한 토큰)을 그대로 넣는다. 모듈
/// 헤더 "상태 배지의 레벨은 서버가 정한다" 참조. 그래서 이 문자열 안에는 `"ok"`·`"warn"`
/// 같은 레벨 리터럴이 하나도 없어야 하고, 아래 테스트가 그것을 고정한다.
/// `data.level`이 문자열인지 확인하고 넣는 이유: 서버가 필드를 빼먹은 구버전 이벤트를
/// 받아도 배지에 `"undefined"`를 박지 않고 마지막으로 알던 레벨을 유지한다.
const RUNNING_SCRIPT: &str = r#"
(function () {
  var root = document.getElementById("backup-run");
  if (!root || !window.EventSource) { return; }
  var source = new EventSource(root.dataset.eventsHref);
  var stateTag = document.getElementById("backup-state");
  var progressText = document.getElementById("backup-progress-text");
  var progressBar = document.getElementById("backup-progress-bar");
  var logBox = document.getElementById("backup-log");
  var doneBanner = document.getElementById("backup-done");

  source.addEventListener("progress", function (e) {
    var data = JSON.parse(e.data);
    if (progressBar) {
      if (typeof data.total === "number") {
        progressBar.max = data.total;
        progressBar.value = data.bytes;
      } else {
        progressBar.removeAttribute("value");
        progressBar.removeAttribute("max");
      }
    }
    if (progressText) {
      progressText.textContent = (typeof data.total === "number")
        ? (data.bytes + " / " + data.total + " bytes")
        : (data.bytes + " bytes");
    }
  });

  source.addEventListener("log", function (e) {
    var data = JSON.parse(e.data);
    if (!logBox) { return; }
    var line = document.createElement("div");
    line.textContent = "[" + data.stream + "] " + data.line;
    logBox.appendChild(line);
    logBox.scrollTop = logBox.scrollHeight;
  });

  function applyLevel(el, level) {
    if (el && typeof level === "string" && level.length > 0) {
      el.setAttribute("data-level", level);
    }
  }

  source.addEventListener("state", function (e) {
    var data = JSON.parse(e.data);
    if (stateTag) {
      stateTag.textContent = data.state;
      applyLevel(stateTag, data.level);
    }
  });

  source.addEventListener("done", function (e) {
    var data = JSON.parse(e.data);
    if (doneBanner) {
      doneBanner.hidden = false;
      doneBanner.textContent = "Finished: " + data.outcome +
        (typeof data.exit_code === "number" ? " (exit " + data.exit_code + ")" : "");
      applyLevel(doneBanner, data.level);
    }
    if (stateTag) {
      stateTag.textContent = data.outcome;
      applyLevel(stateTag, data.level);
    }
    source.close();
  });

  source.onerror = function () {
    if (stateTag) { stateTag.textContent = "disconnected"; }
  };
})();
"#;

/// `GET /backup` 폼 본문.
///
/// `notice`는 이전 제출이 실패했을 때(검증 오류·중복 실행 거부) 위에 얹는 알림 블록이다
/// ([`validation_notice`]·[`already_running_notice`]가 만든다).
pub fn form_body(lang: Lang, profiles: &[String], notice: Option<Markup>) -> Markup {
    html! {
        (components::page_head(BACKUP_TITLE, Some(lang.sel(
            "Run a backup for one profile and watch it live.",
            "프로파일 하나를 골라 백업을 실행하고 실시간으로 지켜봅니다.",
        ))))
        @if let Some(notice) = notice {
            (notice)
        }
        @if profiles.is_empty() {
            div class="card" {
                p { (lang.sel(
                    "No profiles were found in the config file, so there is nothing to back up. Add a profile first.",
                    "config 파일에서 프로파일을 찾지 못해 백업할 대상이 없습니다. 먼저 프로파일을 추가하세요.",
                )) }
            }
        } @else {
            (components::panel(
                html! { h3 class="panel__title" { "New backup" } },
                html! {
                    form method="post" action=(BACKUP_PATH) {
                        div class="field" {
                            label class="field__label mono" for="f-profile" { "profile" }
                            select id="f-profile" name="profile" required {
                                @for profile in profiles {
                                    option value=(profile) { (profile) }
                                }
                            }
                        }
                        div class="field" {
                            label class="field__label mono" for="f-type" { "type" }
                            select id="f-type" name="type" {
                                option value="full" { "full" }
                                option value="incr" { "incr" }
                            }
                        }
                        div class="field" {
                            label class="field__label mono" for="f-db" { "db" }
                            input id="f-db" type="text" name="db" autocomplete="off";
                            p class="field__hint" { (lang.sel(
                                "Optional. Leave empty to back up the whole source.",
                                "선택 항목입니다. 비워 두면 소스 전체를 백업합니다.",
                            )) }
                        }
                        div class="field" {
                            label class="field__label mono" for="f-collection" { "collection" }
                            input id="f-collection" type="text" name="collection" autocomplete="off";
                            p class="field__hint" { (lang.sel(
                                "Optional. Requires db — the child rejects collection without db (exit 2).",
                                "선택 항목입니다. db가 함께 있어야 합니다 — 없으면 자식이 exit 2로 거부합니다.",
                            )) }
                        }
                        div class="field" {
                            label class="field__label mono" for="f-from" { "from" }
                            input id="f-from" type="text" name="from" autocomplete="off";
                            p class="field__hint" { (lang.sel(
                                "Optional. Which read destination to source from (name or type#index).",
                                "선택 항목입니다. 어느 destination에서 읽을지(이름 또는 type#idx).",
                            )) }
                        }
                        div class="field" {
                            label {
                                input type="checkbox" name="no_encrypt" value="1";
                                " no-encrypt"
                            }
                            p class="field__hint" { (lang.sel(
                                "Explicit opt-out of encryption for this run.",
                                "이번 실행에 한해 암호화를 명시적으로 끕니다.",
                            )) }
                        }
                        div class="field" {
                            label {
                                input type="checkbox" name="no_hooks" value="1";
                                " no-hooks"
                            }
                            p class="field__hint" { (lang.sel(
                                "Skip lifecycle hooks for this run.",
                                "이번 실행에 한해 생명주기 훅을 건너뜁니다.",
                            )) }
                        }
                        div class="actions" {
                            button type="submit" { (lang.sel("Start backup", "백업 시작")) }
                        }
                    }
                },
            ))
        }
    }
}

/// 검증 실패 알림(400과 함께 나간다).
pub fn validation_notice(lang: Lang, message: &str) -> Markup {
    components::notice(
        Level::Error,
        lang.sel("Could not start backup", "백업을 시작할 수 없습니다"),
        html! { p { (message) } },
    )
}

/// 같은 프로파일이 이미 실행 중이라 사전 거부됐을 때의 알림(409와 함께 나간다).
pub fn already_running_notice(lang: Lang, running_href: &str) -> Markup {
    components::notice(
        Level::Warn,
        lang.sel("Already running", "이미 실행 중"),
        html! {
            p { (lang.sel(
                "Another backup for this profile is already running. Starting a second one would only fail with a lock conflict — wait for it to finish, or watch it now.",
                "이 프로파일의 다른 백업이 이미 실행 중입니다. 지금 또 시작해도 락 충돌로 끝날 뿐입니다 — 끝나기를 기다리거나, 지금 지켜보세요.",
            )) }
            p { a href=(running_href) { (lang.sel("Watch the running backup", "실행 중인 백업 보기")) } }
        },
    )
}

/// 취소를 시도할 신원 정보(pid·프로파일)가 없을 때의 안내.
pub fn cancel_unavailable(lang: Lang) -> Markup {
    components::notice(
        Level::Error,
        lang.sel("Cannot cancel", "취소할 수 없습니다"),
        html! {
            p { (lang.sel(
                "This job has no recorded pid or profile, so there is nothing safe to signal.",
                "이 잡에는 기록된 pid·프로파일이 없어 안전하게 신호를 보낼 대상이 없습니다.",
            )) }
        },
    )
}

/// 잡 id 형식이 틀렸을 때의 본문(400).
pub fn malformed_id(lang: Lang) -> Markup {
    html! {
        (components::page_head(BACKUP_TITLE, None))
        (components::notice(Level::Error, lang.sel("Malformed job id", "잡 id 형식 오류"), html! {
            p { (lang.sel(
                "That link does not carry a job id. Start a new backup instead.",
                "이 링크에는 잡 id가 없습니다. 새 백업을 시작하세요.",
            )) }
        }))
        p class="muted" { a href=(BACKUP_PATH) { (lang.sel("Back to backup", "백업으로 돌아가기")) } }
    }
}

/// 모르는 잡 id(404).
pub fn unknown_job(lang: Lang) -> Markup {
    html! {
        (components::page_head(BACKUP_TITLE, None))
        (components::notice(Level::Error, lang.sel("Unknown job", "알 수 없는 잡"), html! {
            p { (lang.sel(
                "No history was found for this job id.",
                "이 잡 id에 대한 이력을 찾지 못했습니다.",
            )) }
        }))
        p class="muted" { a href=(BACKUP_PATH) { (lang.sel("Back to backup", "백업으로 돌아가기")) } }
    }
}

/// `GET /backup/{id}` — 실행 중(또는 이미 끝난) 잡의 라이브 화면.
///
/// 초기 렌더는 [`JobDetail`](서버가 이 요청 시점에 파일에서 읽은 스냅샷)로 채우고,
/// [`RUNNING_SCRIPT`]가 그 위에 SSE로 실시간 갱신을 얹는다 — JS가 꺼져 있어도(또는
/// `EventSource`가 없는 아주 오래된 브라우저) 이 초기 렌더 자체는 새로고침마다 최신
/// 스냅샷을 보여주므로 화면이 완전히 죽지는 않는다.
pub fn running_body(
    lang: Lang,
    id: &JobId,
    detail: &JobDetail,
    registry: &SecretRegistry,
) -> Markup {
    let events_href = backup_events_href(id);
    let cancel_href = backup_cancel_href(id);
    let (state_text, state_level) = state_badge(detail);
    html! {
        (components::page_head(BACKUP_TITLE, Some(&id.to_string())))
        div id="backup-run" data-events-href=(events_href) {
            (components::panel(
                html! {
                    h3 class="panel__title" { "Status" }
                    span id="backup-state" class="tag" data-level=(state_level.token()) {
                        (state_text)
                    }
                },
                html! {
                    p id="backup-progress-text" class="mono" { (lang.sel("Waiting for progress…", "진행률 대기 중…")) }
                    progress id="backup-progress-bar" {}
                    p id="backup-done" class="mono" hidden {}
                    form method="post" action=(cancel_href) {
                        div class="actions" {
                            button type="submit" { (lang.sel("Cancel", "취소")) }
                        }
                    }
                    p class="muted" {
                        a href=(job_detail_href(id)) { (lang.sel("View full history entry", "전체 이력 보기")) }
                    }
                },
            ))
            (components::panel(
                html! {
                    h3 class="panel__title" { "Log" }
                    span class="counts" { (detail.logs.len()) " lines" }
                },
                html! {
                    @if detail.logs.is_empty() {
                        p class="muted" { (lang.sel("No log lines yet.", "아직 로그 줄이 없습니다.")) }
                    }
                    pre id="backup-log" class="logdump" {
                        @for line in &detail.logs {
                            (render_log_line(line, registry))
                            "\n"
                        }
                    }
                },
            ))
        }
        script { (PreEscaped(RUNNING_SCRIPT)) }
    }
}

/// 상태 배지의 텍스트와 레벨 — **레벨의 출처는 [`job_exit::level_for_label`] 하나다**
/// (모듈 헤더 "상태 배지의 레벨은 서버가 정한다" 참조).
///
/// 세 갈래다:
///
/// 1. **요약이 없다** — 이 잡에 대해 아는 것이 로그 파일뿐이다(인덱스가 없거나 손상됐다).
///    결과를 모르므로 [`UNKNOWN_STATE_LABEL`] + [`Level::Error`]다. "done"이라고 말하면
///    끝났다는 것까지 아는 척하는 것이다.
/// 2. **아직 돈다**([`crate::web::state::jobs::JobSummary::is_running`]) —
///    [`RUNNING_STATE_LABEL`] + [`Level::Ok`].
///    "지금까지는 문제 없다"가 아는 전부이고, 아직 결과가 없으므로 결과 레벨을 쓸 수 없다.
/// 3. **끝났다** — 결과 라벨을 그대로 배지 글자로 쓰고 레벨은 그 라벨에서 나온다. 그래서
///    succeeded / failed / rejected / lock-conflict가 라이브 화면에서도 **서로 다른 배지**로
///    구분된다(예전에는 전부 `done`/`warn` 하나로 뭉개졌다).
///
/// 배지 글자에 결과 라벨을 그대로 쓰는 것은 이스케이프 관점에서도 안전하다 — 그 값은 t10의
/// 닫힌 어휘([`crate::web::job::JobOutcome::label`])에서만 나오고, 설령 손으로 편집된
/// 인덱스에서 임의 문자열이 들어와도 maud가 자동 이스케이프한다.
fn state_badge(detail: &JobDetail) -> (&str, Level) {
    let Some(summary) = detail.summary.as_ref() else {
        return (UNKNOWN_STATE_LABEL, Level::Error);
    };
    if summary.is_running() {
        return (RUNNING_STATE_LABEL, Level::Ok);
    }
    match summary.outcome.as_deref() {
        Some(label) => (label, job_exit::level_for_label(label)),
        // 종료 시각은 있는데 결과 라벨이 없다 — 저장 경로상 나올 수 없는 조합이지만
        // (`JobSummary::apply_end`가 둘을 함께 채운다) 손으로 편집된 인덱스에서는 가능하다.
        None => (UNKNOWN_STATE_LABEL, Level::Error),
    }
}

/// 로그 한 줄을 마스킹 + 절단해 렌더한다(모듈 헤더 "남의 텍스트를 다루는 규칙").
fn render_log_line(line: &JobLogLine, registry: &SecretRegistry) -> Markup {
    html! {
        (line.ts.format("%H:%M:%S").to_string())
        " "
        (line.stream.token())
        " "
        (truncate(&registry.mask(&line.text)))
    }
}

/// 문자열을 [`MAX_LOG_LINE_CHARS`]로 자른다 — **문자 경계**로 잘라 멀티바이트가 깨지지
/// 않는다([`crate::web::view::jobs::truncate`]와 같은 처리를 독립적으로 둔다 — 그 함수는
/// 비공개다).
fn truncate(text: &str) -> String {
    if text.chars().count() <= MAX_LOG_LINE_CHARS {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX_LOG_LINE_CHARS).collect();
    format!("{head}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::state::jobs::LogStream;
    use chrono::{TimeZone, Utc};

    fn ts(offset_secs: i64) -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_785_000_000 + offset_secs, 0).unwrap()
    }

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    fn sample_detail(running: bool, logs: Vec<JobLogLine>) -> JobDetail {
        if running {
            return detail_with(None, logs);
        }
        detail_with(Some(crate::web::job::JobOutcome::Succeeded), logs)
    }

    /// 결과가 `outcome`인(또는 아직 도는) 잡 하나의 상세를 만든다.
    fn detail_with(
        outcome: Option<crate::web::job::JobOutcome>,
        logs: Vec<JobLogLine>,
    ) -> JobDetail {
        use crate::web::state::jobs::JobSummary;
        let id = JobId::generate();
        let summary = JobSummary {
            id,
            command: "backup".to_string(),
            profile: Some("prod".to_string()),
            args_masked: vec![
                "backup".to_string(),
                "--profile".to_string(),
                "prod".to_string(),
            ],
            started_at: ts(0),
            pid: Some(4242),
            finished_at: outcome.map(|_| ts(30)),
            outcome: outcome.map(|o| o.label().to_string()),
            exit_code: outcome.and_then(|o| o.exit_code()),
        };
        JobDetail {
            id,
            summary: Some(summary),
            logs,
            dropped_leading_logs: 0,
            log_file_present: true,
            unreadable_lines: 0,
        }
    }

    /// 폼은 프로파일 목록을 `<option>`으로 내고, 지정한 필드 전부를 낸다.
    #[test]
    fn form_lists_profiles_and_all_fields() {
        let out =
            form_body(Lang::En, &["prod".to_string(), "staging".to_string()], None).into_string();
        assert!(
            out.contains(r#"<option value="prod">prod</option>"#),
            "{out}"
        );
        assert!(out.contains(r#"<option value="staging">staging</option>"#));
        for name in [
            "profile",
            "type",
            "db",
            "collection",
            "from",
            "no_encrypt",
            "no_hooks",
        ] {
            assert!(
                out.contains(&format!(r#"name="{name}""#)),
                "'{name}' 필드 누락: {out}"
            );
        }
        assert!(out.contains(r#"method="post""#));
        assert!(out.contains(r#"action="/backup""#));
    }

    /// 프로파일이 없으면 폼 대신 안내만 나온다(빈 select로 제출 가능한 상태를 만들지 않는다).
    #[test]
    fn empty_profiles_shows_placeholder_not_a_form() {
        let out = form_body(Lang::En, &[], None).into_string();
        assert!(out.contains("No profiles were found"));
        assert!(!out.contains("<form"), "빈 목록인데 폼이 나왔다: {out}");
    }

    /// notice가 있으면 폼 위에 그려진다.
    #[test]
    fn notice_is_rendered_above_the_form() {
        let notice = already_running_notice(Lang::En, "/backup/abc/events");
        let out = form_body(Lang::En, &["prod".to_string()], Some(notice)).into_string();
        assert!(out.contains("Already running"));
        assert!(out.contains(r#"href="/backup/abc/events""#));
        assert!(out.contains(r#"data-level="warn""#));
    }

    /// 실행 중 화면은 SSE 경로를 `data-events-href`로 싣고, 취소 폼·이력 링크를 낸다.
    #[test]
    fn running_body_carries_sse_href_and_cancel_form() {
        let detail = sample_detail(true, vec![]);
        let id = detail.id;
        let out = running_body(Lang::En, &id, &detail, &empty_registry()).into_string();
        assert!(
            out.contains(&format!(r#"data-events-href="/backup/{id}/events""#)),
            "{out}"
        );
        assert!(out.contains(&format!(r#"action="/backup/{id}/cancel""#)));
        assert!(
            out.contains(&format!(r#"href="/jobs/{id}""#)),
            "이력 링크 누락: {out}"
        );
        assert!(out.contains(r#"data-level="ok""#), "실행 중 배지 누락");
    }

    /// 끝난 잡의 상태 배지는 **결과 라벨과 그 결과의 레벨**을 보여준다.
    ///
    /// ## 이 테스트는 예전에 틀린 동작을 고정하고 있었다
    /// 원래 이 테스트는 exit 0으로 끝난 잡에 대해 `>done<`을 단정하고, doc 주석은 그것을
    /// "done/warn으로 보인다(실패로 단정하지 않는다)"고 정당화했다. 두 군데가 틀렸다:
    ///
    /// 1. **배지가 `warn`이었다.** exit 0은 흠 없는 성공이다. 성공을 경고색으로 칠하면
    ///    운영자가 멀쩡한 백업을 다시 돌린다 — t13이 막으려 한 실패 모드의 거울상이다.
    /// 2. **모든 종료가 `done` 하나로 뭉개졌다.** succeeded / failed / rejected /
    ///    lock-conflict가 라이브 화면에서 전혀 구분되지 않았다. `summary.outcome`이 이미
    ///    손에 있는데 쓰지 않았다.
    ///
    /// "실패로 단정하지 않는다"는 의도 자체는 옳았지만, 그 해법이 "전부 경고로 칠한다"였다.
    /// 올바른 해법은 결과를 그대로 말하는 것이다.
    #[test]
    fn finished_job_shows_its_outcome_not_a_generic_done() {
        let detail = sample_detail(false, vec![]);
        let id = detail.id;
        let out = running_body(Lang::En, &id, &detail, &empty_registry()).into_string();
        assert!(
            out.contains(">succeeded<"),
            "결과 라벨이 배지에 없다: {out}"
        );
        assert!(!out.contains(">done<"), "결과가 'done'으로 뭉개졌다: {out}");
        assert!(
            out.contains(r#"data-level="ok""#),
            "exit 0 성공이 ok로 표시되지 않았다: {out}"
        );
        assert!(
            !out.contains(r#"data-level="warn""#),
            "exit 0 성공이 경고로 칠해졌다: {out}"
        );
    }

    /// **H3 핵심**: 종료 상태가 라이브 화면에서 배지로 서로 구분된다 — 특히 exit 0은
    /// `ok`이고 `warn`이 아니다.
    ///
    /// 아홉 결과를 전부 그려 (a) 배지 글자가 결과 라벨이고 (b) `data-level`이 t13의
    /// [`job_exit::level_for_label`] 값과 같은지 본다. 레벨은 네 칸뿐이라 여러 결과가 색을
    /// 공유하지만(exit 4와 exit 5는 둘 다 `warn`) **글자는 겹치지 않으므로** 운영자가 배지만
    /// 보고도 무엇이 일어났는지 구분할 수 있다.
    #[test]
    fn every_outcome_renders_its_own_badge_on_the_live_screen() {
        use crate::web::job::JobOutcome;
        let outcomes = [
            JobOutcome::Succeeded,
            JobOutcome::SucceededWithWarnings,
            JobOutcome::LockConflict,
            JobOutcome::Failed,
            JobOutcome::Rejected,
            JobOutcome::PrecheckFailed,
            JobOutcome::UnexpectedExit(42),
            JobOutcome::Signaled(15),
            JobOutcome::Unknown,
        ];
        let mut seen_labels = Vec::new();
        for outcome in outcomes {
            let detail = detail_with(Some(outcome), vec![]);
            let id = detail.id;
            let out = running_body(Lang::En, &id, &detail, &empty_registry()).into_string();
            let label = outcome.label();
            let expected = job_exit::level_for_label(label);
            assert!(
                out.contains(&format!(">{label}<")),
                "{outcome:?}의 결과 라벨이 배지에 없다: {out}"
            );
            assert!(
                out.contains(&format!(r#"data-level="{}""#, expected.token())),
                "{outcome:?}의 레벨이 {}가 아니다: {out}",
                expected.token()
            );
            seen_labels.push(label);
        }
        // 아홉 결과의 배지 글자가 전부 다르다 — 색이 겹쳐도 글자로는 구분된다.
        seen_labels.sort_unstable();
        let total = seen_labels.len();
        seen_labels.dedup();
        assert_eq!(seen_labels.len(), total, "배지 글자가 겹치는 결과가 있다");
    }

    /// exit 0(성공)·exit 4(경고 동반 성공)·exit 5(락 충돌)·exit 1(실패)·exit 2(사용법
    /// 오류)가 라이브 화면에서 **의미가 다른 만큼 다른 레벨**로 나온다.
    ///
    /// 위 테스트가 "레벨이 t13과 같은가"를 보는 것과 달리, 여기서는 그 레벨들이 실제로
    /// 서로 갈라지는지를 본다 — 매핑이 통째로 한 값으로 뭉개져도 위 테스트는 통과한다.
    #[test]
    fn success_warning_and_failure_are_visually_distinct() {
        use crate::web::job::JobOutcome;
        let level_of = |outcome: JobOutcome| -> Level {
            let detail = detail_with(Some(outcome), vec![]);
            let out = running_body(Lang::En, &detail.id, &detail, &empty_registry()).into_string();
            // 상태 배지 **태그 안**만 본다 — 화면 다른 곳의 `data-level`(알림 블록 등)을
            // 잘못 읽지 않도록 여는 태그가 닫히는 `>`까지로 범위를 좁힌다.
            let (_, tail) = out
                .split_once(r#"id="backup-state""#)
                .expect("상태 배지가 없다");
            let (tag, _) = tail.split_once('>').expect("배지 태그가 닫히지 않았다");
            for level in [Level::Ok, Level::Warn, Level::Fail, Level::Error] {
                if tag.contains(&format!(r#"data-level="{}""#, level.token())) {
                    return level;
                }
            }
            panic!("상태 배지의 레벨을 찾지 못했다: {out}");
        };

        let success = level_of(JobOutcome::Succeeded);
        let warned = level_of(JobOutcome::SucceededWithWarnings);
        let locked = level_of(JobOutcome::LockConflict);
        let failed = level_of(JobOutcome::Failed);
        let rejected = level_of(JobOutcome::Rejected);

        assert_eq!(success, Level::Ok, "exit 0은 ok여야 한다");
        assert_ne!(success, warned, "exit 0과 exit 4가 같은 레벨이다");
        assert_ne!(
            warned, failed,
            "exit 4(성공)와 exit 1(실패)이 같은 레벨이다"
        );
        assert_ne!(locked, failed, "exit 5(락 충돌)가 실패와 같은 레벨이다");
        assert_ne!(failed, rejected, "exit 1과 exit 2가 같은 레벨이다");
        // 성공 계열은 절대 실패색이 아니다.
        for level in [success, warned] {
            assert_ne!(level, Level::Fail);
            assert_ne!(level, Level::Error);
        }
    }

    /// 아직 도는 잡은 `running`/`ok`다 — 결과가 없으므로 결과 레벨을 쓰지 않는다.
    #[test]
    fn running_job_shows_running_not_an_outcome() {
        let detail = detail_with(None, vec![]);
        let out = running_body(Lang::En, &detail.id, &detail, &empty_registry()).into_string();
        assert!(out.contains(&format!(">{RUNNING_STATE_LABEL}<")), "{out}");
        assert!(out.contains(r#"data-level="ok""#));
    }

    /// 요약이 아예 없는 잡(인덱스 손상·부재)은 `unknown`/`error`다 — "done"이라고 말하면
    /// 끝났다는 것까지 아는 척하는 것이다.
    #[test]
    fn job_without_a_summary_is_unknown_not_done() {
        let mut detail = detail_with(None, vec![]);
        detail.summary = None;
        let out = running_body(Lang::En, &detail.id, &detail, &empty_registry()).into_string();
        assert!(out.contains(&format!(">{UNKNOWN_STATE_LABEL}<")), "{out}");
        assert!(out.contains(r#"data-level="error""#), "{out}");
    }

    /// [`UNKNOWN_STATE_LABEL`]이 결과 어휘 안의 값이다 — 그래야 레벨 매핑이 이 값을 알아본다.
    #[test]
    fn unknown_state_label_matches_the_outcome_vocabulary() {
        assert_eq!(
            UNKNOWN_STATE_LABEL,
            crate::web::job::JobOutcome::Unknown.label()
        );
        assert_eq!(
            job_exit::level_for_label(UNKNOWN_STATE_LABEL),
            Level::Error,
            "모르는 라벨로 떨어지면 안 된다 — 어휘 안의 값이어야 한다"
        );
    }

    /// **스크립트가 레벨을 스스로 정하지 않는다.**
    ///
    /// 레벨 매핑이 Rust와 JS 두 언어로 갈라지면 다시 어긋난다 — 실제로 그랬다(스크립트가
    /// 종료 시 무조건 `"warn"`을 박아 exit 0 성공이 경고로 떴다). 그래서 이 스크립트 안에는
    /// [`Level::token`] 어휘의 리터럴이 하나도 없어야 하고, 서버가 보낸 `data.level`만 써야
    /// 한다.
    #[test]
    fn script_does_not_decide_levels_itself() {
        for level in [Level::Ok, Level::Warn, Level::Fail, Level::Error] {
            let literal = format!("\"{}\"", level.token());
            assert!(
                !RUNNING_SCRIPT.contains(&literal),
                "스크립트가 레벨 리터럴 {literal}을 직접 들고 있다 — 서버가 내려보낸 값만 \
                 써야 한다"
            );
        }
        assert!(
            RUNNING_SCRIPT.contains("data.level"),
            "스크립트가 서버가 보낸 레벨을 읽지 않는다"
        );
        assert!(
            RUNNING_SCRIPT.contains("data.outcome"),
            "종료 시 배지 글자를 결과 라벨로 바꾸지 않는다"
        );
    }

    /// 로그 줄은 마스킹되고, 상한을 넘으면 절단된다.
    #[test]
    fn log_lines_are_masked_and_truncated() {
        const FAKE: &str = "NOT-A-REAL-SECRET-backupview-4f2a91";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(FAKE));
        let long = "A".repeat(5_000);
        let logs = vec![
            JobLogLine {
                ts: ts(1),
                stream: LogStream::Stderr,
                text: format!("auth failed {FAKE}"),
            },
            JobLogLine {
                ts: ts(2),
                stream: LogStream::Stderr,
                text: long.clone(),
            },
        ];
        let detail = sample_detail(true, logs);
        let id = detail.id;
        let out = running_body(Lang::En, &id, &detail, &registry).into_string();
        assert!(!out.contains(FAKE), "시크릿이 남았다: {out}");
        assert!(out.contains(crate::web::mask::REDACTED_PLACEHOLDER));
        assert!(!out.contains(&long), "절단되지 않았다");
        assert!(out.contains(&"A".repeat(MAX_LOG_LINE_CHARS)));
        assert!(out.contains('…'));
    }

    /// 적대적 로그(HTML 태그·제어문자)가 이스케이프된다.
    #[test]
    fn hostile_log_content_is_escaped() {
        let hostile = r#"<script>alert(1)</script>"#;
        let logs = vec![JobLogLine {
            ts: ts(1),
            stream: LogStream::Stderr,
            text: hostile.to_string(),
        }];
        let detail = sample_detail(true, logs);
        let id = detail.id;
        let out = running_body(Lang::En, &id, &detail, &empty_registry()).into_string();
        assert!(
            !out.contains("<script>alert"),
            "이스케이프되지 않았다: {out}"
        );
        assert!(out.contains("&lt;script&gt;"));
    }

    /// 컴파일 타임 스크립트는 그대로 나가되(PreEscaped), 그 자리를 제외하면 마크업에
    /// 색 리터럴·인라인 스타일이 없다(모듈 헤더 계약).
    #[test]
    fn markup_carries_no_presentation() {
        let detail = sample_detail(
            true,
            vec![JobLogLine {
                ts: ts(1),
                stream: LogStream::Stderr,
                text: "plain line".to_string(),
            }],
        );
        let id = detail.id;
        let rendered = [
            form_body(Lang::En, &["prod".to_string()], None).into_string(),
            running_body(Lang::En, &id, &detail, &empty_registry()).into_string(),
            malformed_id(Lang::Ko).into_string(),
            unknown_job(Lang::Ko).into_string(),
            cancel_unavailable(Lang::En).into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            // 다른 뷰 테스트(`components`·`jobs`)와 달리 여기서는 순수 `!contains('#')`를
            // 쓰지 않는다 — 폼 힌트 문구가 `--from` 값 표기(`type#idx`)를 정당하게
            // 담고 있어(`job::spec::JobSpec::with_from` 어휘) `#`이 색 리터럴이 아니라
            // 콘텐츠로 나타난다. 대신 실제로 위험한 형태(CSS hex 색상: `#` 뒤에 3·6자리
            // 16진수)만 좁혀서 잡는다.
            assert!(
                !hex_color_literal(&out),
                "색 리터럴로 보이는 값이 있다: {out}"
            );
        }
    }

    /// `#` 뒤에 3자리 또는 6자리 16진수가 바로 이어지는 패턴(CSS hex 색상 리터럴)이
    /// 있는지 본다. `type#idx`처럼 `#` 뒤에 16진수가 아닌 문자가 오는 정당한 콘텐츠는
    /// 걸리지 않는다.
    fn hex_color_literal(text: &str) -> bool {
        let bytes = text.as_bytes();
        for (i, &b) in bytes.iter().enumerate() {
            if b != b'#' {
                continue;
            }
            let rest = &bytes[i + 1..];
            let hex_run = rest.iter().take_while(|c| c.is_ascii_hexdigit()).count();
            if hex_run == 3 || hex_run == 6 {
                return true;
            }
        }
        false
    }
}
