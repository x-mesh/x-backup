//! `verify` 화면의 마크업 — 폼과 결과.
//!
//! ## 이 파일이 하는 일 / 하지 않는 일
//! [`crate::web::routes`] 모듈 헤더의 규약대로 **순수 렌더 함수**만 담는다 — 자식
//! 프로세스도, 파일시스템도, 잡 저장소도 직접 만지지 않는다. 실행·이력 읽기·argv 파싱은
//! 전부 [`crate::web::routes::verify`](이하 `route`)의 몫이고, 이 파일은 그 결과([`JobDetail`]·
//! [`route::VerifyDoc`]·경로 문자열)만 받아 마크업으로 접는다. `view`가 `routes`를
//! import하는 방향은 `view::backup`이 `routes::backup`을 쓰는 것과 같은 이 코드베이스의
//! 기존 관례다.
//!
//! ## 결과 판정에 `job_exit::present()`를 쓰지 못하는 이유
//! [`job_exit::present`]는 **라이브 [`JobOutcome`] 값**을 요구한다 — 그 파일 헤더가 명시적
//! 근거를 남겼다: `Signaled(i32)`의 시그널 번호가 문자열 라벨만으로는 복원되지 않으므로
//! `label → JobOutcome` 역변환을 애초에 만들지 않았다. 이 화면(`GET /verify/{job_id}`)은
//! **저장소에서 다시 읽은 문자열 라벨**([`JobDetail::summary`]→`outcome`)만 갖고 있고, 그
//! 잡을 막 끝낸 요청도 아니다(`route::start_verify_job`의 백그라운드 태스크가 이미 끝내고
//! 사라진 뒤일 수 있다). 그래서 [`crate::web::view::jobs`]와 같은 결정을 내린다: **레벨(색)
//! 은 [`job_exit::level_for_label`] 하나로 통일**하고(중복 매핑 없음), **문구는 이 화면이
//! 직접 쓴다**([`outcome_headline`]·[`outcome_detail`]).
//!
//! 그 문구가 지켜야 하는 것 하나 — **verify의 exit 4는 backup의 exit 4와 다른 뜻이다.**
//! backup의 exit 4는 "oplog gap이 감지되어 증분이 풀백업으로 승격됐다"이고, verify의 exit
//! 4는 "검증이 끝까지 실행됐고 이 백업에서 표시할 문제(불완전한 manifest, 끊긴 체인 등)를
//! 찾았다"이다. 둘 다 "경고 동반 성공"이라는 공통점 때문에 배지 색은 같지만(둘 다
//! [`Level::Warn`]), 화면을 오가는 운영자가 색만 보고 같은 사건이라고 오해하지 않도록
//! [`outcome_detail`]이 그 구분을 명시한다.
//!
//! ## 모양은 담지 않는다 — 새 CSS 클래스를 만들지 않는다
//! [`components`] 헤더의 계약을 그대로 따른다. 표는 `.dtable`(잡 이력·doctor 화면이 이미
//! 쓰는 클래스), 폼은 `.field`/`.actions`(`view::backup`·`view::config`와 동일), 로그
//! 발췌는 `.logdump`를 재사용한다. 상태는 `data-level` 토큰으로만 말한다.
//!
//! ## 자동 새로고침 — 왜 인라인 `<script>`인가
//! [`layout::shell`]의 `<head>`는 고정이라(이 태스크가 손댈 수 없는 파일) 메타 리프레시
//! 태그를 넣을 자리가 없다. `view::backup::RUNNING_SCRIPT`와 같은 이유로, 본문(`<main>`)
//! 안에 컴파일 타임 상수 `<script>`를 하나 둔다([`AUTO_REFRESH_SCRIPT`]) — 사용자 입력을
//! 한 글자도 보간하지 않으므로 `PreEscaped`를 써도 안전하다([`components`] 헤더의
//! "`PreEscaped`는 컴파일 타임 상수에만" 규약).
//!
//! ## 남의 텍스트를 다루는 두 가지 규칙(잡 이력 화면과 동일)
//! stdout·stderr·경고 문자열은 자식 프로세스가 만든 문자열이다. 그래서: (1)
//! [`SecretRegistry::mask`]를 무조건 한 번 더 통과시킨다(쓰는 쪽이 이미 지웠어도 — 방어
//! 심층화), (2) 발췌 길이를 문자 경계에서 자른다([`MAX_STDERR_CHARS`]). 이스케이프는 maud의
//! 자동 처리에 맡긴다 — [`AUTO_REFRESH_SCRIPT`] 외에는 `PreEscaped`가 없다.

use maud::{html, Markup, PreEscaped};

use crate::i18n::Lang;
use crate::web::job::exit as job_exit;
use crate::web::mask::SecretRegistry;
use crate::web::routes::verify as route;
use crate::web::state::jobs::{JobDetail, JobId, LogStream};
use crate::web::view::components::{self, Level};

/// stdout·stderr 발췌 상한(문자 수). 잡 이력 화면의 [`crate::web::view::jobs::MAX_TEXT_CHARS`]
/// 와 같은 근거·같은 값이지만, verify 결과의 stdout은 JSON 한 줄(구조화 렌더로 이미
/// 소화된다)이고 화면에 직접 발췌되는 것은 stderr뿐이므로 독립 상수로 둔다.
pub const MAX_STDERR_CHARS: usize = 2_000;

/// 진행 중 화면에 붙는 자동 새로고침 스크립트 — 컴파일 타임 상수(모듈 헤더 참조).
///
/// 4초 간격의 근거: 구조 검증(보통 수백 ms 이내)이 한 번의 갱신 안에 대개 끝나면서도,
/// 사람이 화면 앞에서 보기에 초조하게 깜빡이지 않는 값이다. deep 검증처럼 오래 걸리는
/// 잡은 여러 번 갱신을 거치는 것이 당연하다 — 매 갱신은 같은 GET을 다시 부르는 것뿐이라
/// 서버에 주는 부담도 이력 조회 한 번(파일 읽기)뿐이다. `format!`으로 조립하지 않고 고정
/// 리터럴로 두는 이유: 이 문자열을 [`PreEscaped`]로 감싸는 것은 "사용자 입력이 전혀
/// 섞이지 않는다"는 성질에 기대는데, 조립하면 그 성질이 육안으로 덜 분명해진다(값 자체는
/// 여전히 컴파일 타임 상수이므로 안전하지만, 리터럴 하나가 더 명백하다).
///
/// ## 보는 사람이 없으면 멈춘다
/// `document.hidden`이면 새로고침하지 않고 **다시 보일 때까지 기다린다**. 이 콘솔에는
/// 서버측 폴링 루프가 없으므로(모든 갱신이 요청에서 시작한다), "뷰어 없으면 폴링을
/// 멈춘다"가 실제로 걸리는 자리는 여기 하나다 — 잊고 열어 둔 탭이 밤새 4초마다 이력을
/// 다시 읽는 것을 막는다.
///
/// 다시 보이는 순간 **즉시** 새로고침하는 이유: 화면을 다시 본 사람이 가장 먼저 원하는
/// 것이 지금 상태이고, 그 자리에서 4초를 더 기다리게 하면 멈춰 있는 화면으로 읽힌다.
const AUTO_REFRESH_SCRIPT: &str = r#"
(function () {
  function tick() {
    if (document.hidden) {
      document.addEventListener("visibilitychange", function again() {
        document.removeEventListener("visibilitychange", again);
        location.reload();
      });
      return;
    }
    location.reload();
  }
  setTimeout(tick, 4000);
})();
"#;

// ---------------------------------------------------------------------------
// 폼
// ---------------------------------------------------------------------------

/// `GET /verify` 폼 본문.
///
/// `prefill_id`는 카탈로그 화면 링크(`?id=`)나 실패한 이전 제출에서 온 값이다.
/// `target_profile`은 정보성으로만 보여준다 — verify에는 `--profile` 인자가 없으므로(모듈
/// 헤더 `route` doc 참고) 고를 수 있는 것처럼 그리면 운영자가 착각한다.
pub fn form_body(
    lang: Lang,
    prefill_id: Option<&str>,
    target_profile: &str,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::VERIFY_TITLE, Some(lang.sel(
            "Check a backup's integrity — structural (default), deep (decrypt), or chain (continuity).",
            "백업 무결성을 확인합니다 — 구조(기본)·심층(복호화)·체인(연속성) 중 선택하세요.",
        ))))
        @if let Some(notice) = notice {
            (notice)
        }
        (components::meta_list(&[("target profile", target_profile.to_string())]))
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("Run verify", "검증 실행")) } },
            html! {
                form method="post" action=(route::VERIFY_PATH) {
                    div class="field" {
                        label class="field__label mono" for="f-id" { "id" }
                        input id="f-id" type="text" name="id" autocomplete="off" required
                            value=(prefill_id.unwrap_or_default());
                        p class="field__hint" { (lang.sel(
                            "The backup id to check (UUID). Follow a link from the catalog, or paste one.",
                            "확인할 백업 id(UUID)입니다. 카탈로그 링크를 따라오거나 직접 붙여넣으세요.",
                        )) }
                    }
                    div class="field" {
                        label {
                            input type="checkbox" name="deep" value="1";
                            " deep"
                        }
                        p class="field__hint" { (lang.sel(
                            "Decrypt and decompress the backup end to end to confirm it decodes. Needs a decryption key on this host — structural checks below do not.",
                            "백업을 끝까지 복호화·압축해제해 디코드가 되는지 확인합니다. 이 호스트에 복호화 키가 있어야 합니다 — 아래 구조 검증은 필요 없습니다.",
                        )) }
                    }
                    div class="field" {
                        label {
                            input type="checkbox" name="chain" value="1";
                            " chain"
                        }
                        p class="field__hint" { (lang.sel(
                            "Also check base + incremental continuity for point-in-time recovery.",
                            "PITR을 위한 base + 증분 연속성도 함께 확인합니다.",
                        )) }
                    }
                    div class="actions" {
                        button type="submit" { (lang.sel("Run verify", "검증 실행")) }
                    }
                }
            },
        ))
    }
}

/// 제출 검증 실패 알림(400/500과 함께 나간다).
pub fn validation_notice(lang: Lang, message: &str) -> Markup {
    components::notice(
        Level::Error,
        lang.sel("Could not start verify", "검증을 시작할 수 없습니다"),
        html! { p { (message) } },
    )
}

/// 잡 id 형식이 틀렸을 때의 본문(400).
pub fn malformed_id(lang: Lang) -> Markup {
    html! {
        (components::page_head(route::VERIFY_TITLE, None))
        (components::notice(Level::Error, lang.sel("Malformed job id", "잡 id 형식 오류"), html! {
            p { (lang.sel(
                "That link does not carry a job id. Start a new verify instead.",
                "이 링크에는 잡 id가 없습니다. 새 검증을 시작하세요.",
            )) }
        }))
        p class="muted" { a href=(route::VERIFY_PATH) { (lang.sel("Back to verify", "검증으로 돌아가기")) } }
    }
}

/// 모르는 잡 id(404).
pub fn unknown_job(lang: Lang) -> Markup {
    html! {
        (components::page_head(route::VERIFY_TITLE, None))
        (components::notice(Level::Error, lang.sel("Unknown job", "알 수 없는 잡"), html! {
            p { (lang.sel(
                "No history was found for this job id.",
                "이 잡 id에 대한 이력을 찾지 못했습니다.",
            )) }
        }))
        p class="muted" { a href=(route::VERIFY_PATH) { (lang.sel("Back to verify", "검증으로 돌아가기")) } }
    }
}

// ---------------------------------------------------------------------------
// 결과
// ---------------------------------------------------------------------------

/// `GET /verify/{job_id}` 본문 — 진행 중이면 대기 화면, 끝났으면 결과를 그린다.
pub fn result_body(
    lang: Lang,
    id: &JobId,
    detail: &JobDetail,
    registry: &SecretRegistry,
) -> Markup {
    let Some(summary) = &detail.summary else {
        // `routes::verify::result`가 이미 요약도 로그 파일도 없는 경우 404로 끊으므로,
        // 여기 도달하는 것은 "로그 파일은 있는데 요약을 복원하지 못한" 극히 드문 경우뿐이다
        // (`JobStore::detail` doc — 손상된 시작 레코드 등). 패닉하지 않고 읽을 수 있는
        // 오류로 접는다.
        return html! {
            (components::page_head(route::VERIFY_TITLE, None))
            (components::notice(Level::Error, lang.sel("No summary", "요약 없음"), html! {
                p { (lang.sel(
                    "This job has a log file but no readable summary.",
                    "이 잡은 로그 파일은 있지만 읽을 수 있는 요약이 없습니다.",
                )) }
            }))
        };
    };

    if summary.is_running() {
        return running_body(lang, id);
    }

    let outcome_label = summary.outcome.as_deref().unwrap_or("unknown");
    let level = job_exit::level_for_label(outcome_label);
    let deep_requested = route::args_have_flag(&summary.args_masked, "--deep");
    let chain_requested = route::args_have_flag(&summary.args_masked, "--chain");
    let backup_id = route::arg_value(&summary.args_masked, "--id");
    // 실패 상태에서도 재시도 가능한지는 라벨 하나로 판정된다 — `JobOutcome::retryable`은
    // `LockConflict`(라벨 "lock-conflict")에서만 참이고, 그 매핑은 t10의 정의와 1:1이라
    // 갈라질 수 없다(`route` 모듈 헤더 "verify가 락을 잡지 않는다" 참고).
    let retryable = outcome_label == "lock-conflict";

    // 쓰는 쪽(routes::verify)이 이미 한 번 마스킹했지만, 여기서 다시 한 번 통과시킨다
    // (방어 심층화 — `crate::web::state::jobs` 헤더).
    let stdout_text = collect_stream(detail, LogStream::Stdout, registry);
    let stderr_text = collect_stream(detail, LogStream::Stderr, registry);

    // exit 0/4 에서만 결과 문서가 나온다(`route` 모듈 헤더 "이유는 명확히" 참고) — 그 밖의
    // 결과에서 파싱이 실패하는 것은 **정상**이므로 이례로 다루지 않는다. exit 0/4인데
    // 파싱이 실패하면 그건 진짜 이례(스키마 드리프트·손상)이므로 레벨을 강제로
    // [`Level::Error`]로 올린다(`routes::doctor`의 `Outcome::Unreadable`과 같은 판단).
    let json_expected = matches!(outcome_label, "succeeded" | "succeeded-with-warnings");

    html! {
        (components::page_head(route::VERIFY_TITLE, Some(lang.sel(
            "Backup integrity check result.",
            "백업 무결성 검증 결과.",
        ))))
        (meta_row(backup_id, deep_requested, chain_requested))
        @match route::parse_verify_doc(&stdout_text) {
            Ok(doc) => (report_panel(lang, level, outcome_label, &doc)),
            Err(problem) if json_expected => (unreadable_report_panel(lang, &problem, &stderr_text)),
            Err(problem) => (failure_panel(lang, level, outcome_label, deep_requested, &problem, &stderr_text)),
        }
        @if retryable {
            p class="muted" { a href=(retry_href(backup_id)) { (lang.sel("Run this check again", "이 검증 다시 실행")) } }
        }
        p class="muted" { a href=(route::VERIFY_PATH) { (lang.sel("Run another verify", "다른 검증 실행")) } }
    }
}

/// 재시도 링크 — 백업 id를 프리필한 폼으로 되돌린다(모드 선택은 사용자가 다시 한다;
/// 어느 모드로 재시도할지는 원래 요청과 같을 필요가 없다).
fn retry_href(backup_id: Option<&str>) -> String {
    match backup_id {
        Some(id) => format!("{}?id={id}", route::VERIFY_PATH),
        None => route::VERIFY_PATH.to_string(),
    }
}

/// 진행 중 화면.
fn running_body(lang: Lang, id: &JobId) -> Markup {
    let href = route::verify_result_href(id);
    html! {
        (components::page_head(route::VERIFY_TITLE, None))
        (components::notice(Level::Ok, lang.sel("Verification in progress", "검증 진행 중"), html! {
            p { (lang.sel(
                "The check is still running. This page refreshes automatically every few seconds.",
                "검증이 아직 진행 중입니다. 이 화면은 몇 초마다 자동으로 새로고침됩니다.",
            )) }
            p class="muted" { a href=(href) { (lang.sel("Refresh now", "지금 새로고침")) } }
        }))
        script { (PreEscaped(AUTO_REFRESH_SCRIPT)) }
    }
}

/// 메타 정보 줄 — 백업 id·모드.
fn meta_row(backup_id: Option<&str>, deep: bool, chain: bool) -> Markup {
    let mode = match (deep, chain) {
        (true, true) => "deep + chain",
        (true, false) => "deep",
        (false, true) => "chain",
        (false, false) => "structural",
    };
    components::meta_list(&[
        ("backup_id", backup_id.unwrap_or("-").to_string()),
        ("mode", mode.to_string()),
    ])
}

/// 결과 문서를 성공적으로 읽었을 때의 패널 — 각 검사를 **개별 행**으로 보여준다(하나로
/// 뭉개지 않는다).
fn report_panel(lang: Lang, level: Level, outcome_label: &str, doc: &route::VerifyDoc) -> Markup {
    html! {
        (components::verdict_banner(level, &outcome_headline(lang, outcome_label), outcome_detail(lang, outcome_label).as_deref()))
        (components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("Structural checks", "구조 검증")) }
                (components::badge(if doc.ok { Level::Ok } else { Level::Fail }))
            },
            checks_table(lang, doc),
        ))
        @if !doc.warnings.is_empty() {
            (components::notice(Level::Warn, lang.sel("Warnings", "경고"), html! {
                ul { @for w in &doc.warnings { li { (w) } } }
            }))
        }
        @if let Some(chain) = &doc.chain {
            (chain_panel(lang, chain))
        }
    }
}

/// 구조(+심층) 검사 표. `manifest_sidecar_ok`·`data_checksum_ok`·`deep_decode_ok`를 각각
/// 다른 행으로 그린다 — 어느 검사가 통과/실패했는지가 이 화면의 정보다.
fn checks_table(lang: Lang, doc: &route::VerifyDoc) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "Check" }
                        th scope="col" { "Detail" }
                    }
                }
                tbody {
                    (check_row(
                        "manifest_sidecar_ok",
                        doc.manifest_sidecar_ok,
                        lang.sel(
                            "Manifest sidecar checksum matches — the manifest itself was not tampered with.",
                            "manifest 사이드카 체크섬이 일치합니다 — manifest 자체가 변조되지 않았습니다.",
                        ),
                    ))
                    (check_row(
                        "data_checksum_ok",
                        doc.data_checksum_ok,
                        if doc.empty_slice {
                            lang.sel(
                                "data.bin absent — expected for an empty incremental slice (no changes to store).",
                                "data.bin 없음 — 빈 증분 슬라이스(저장할 변경분 없음)라 정상입니다.",
                            )
                        } else {
                            lang.sel(
                                "Recomputed checksum of the stored bytes matches the manifest.",
                                "저장된 바이트의 재계산 체크섬이 manifest와 일치합니다.",
                            )
                        },
                    ))
                    @match doc.deep_decode_ok {
                        Some(ok) => (check_row(
                            "deep_decode_ok",
                            ok,
                            lang.sel(
                                "Decrypt + decompress ran to completion (--deep).",
                                "복호화·압축해제가 끝까지 실행됐습니다(--deep).",
                            ),
                        )),
                        None => tr {
                            td { "\u{2014}" }
                            td class="mono key" { "deep_decode_ok" }
                            td class="msg muted" { (lang.sel(
                                "not requested (run with --deep to check decoding)",
                                "요청되지 않음(디코드까지 확인하려면 --deep으로 실행)",
                            )) }
                        },
                    }
                }
            }
        }
    }
}

fn check_row(key: &str, ok: bool, detail: &str) -> Markup {
    let level = if ok { Level::Ok } else { Level::Fail };
    html! {
        tr data-level=(level.token()) {
            td { (components::badge(level)) }
            td class="mono key" { (key) }
            td class="msg" { (detail) }
        }
    }
}

/// `--chain` 결과 패널.
fn chain_panel(lang: Lang, chain: &route::ChainDoc) -> Markup {
    let level = if chain.continuous {
        Level::Ok
    } else {
        Level::Fail
    };
    let continuity_text = if chain.continuous {
        lang.sel("continuous (PITR available)", "연속(PITR 가능)")
            .to_string()
    } else {
        lang.sel(
            &format!(
                "broken ({} breaks) \u{2014} PITR unavailable",
                chain.breaks.len()
            ),
            &format!("끊김({}건) \u{2014} PITR 불가", chain.breaks.len()),
        )
        .to_string()
    };
    html! {
        (components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("Chain continuity", "체인 연속성")) }
                (components::badge(level))
            },
            html! {
                (components::meta_list(&[
                    ("base_id", chain.base_id.clone()),
                    ("incremental_ids", format!("{}: {}", chain.incremental_ids.len(), chain.incremental_ids.join(", "))),
                    ("continuous", continuity_text),
                ]))
                @if !chain.breaks.is_empty() {
                    (components::notice(Level::Fail, lang.sel("Breaks", "끊긴 지점"), html! {
                        ul { @for b in &chain.breaks { li class="mono" { (b) } } }
                    }))
                }
                @if !chain.warnings.is_empty() {
                    (components::notice(Level::Warn, lang.sel("Chain warnings", "체인 경고"), html! {
                        ul { @for w in &chain.warnings { li { (w) } } }
                    }))
                }
            },
        ))
    }
}

/// exit 0/4인데 결과 문서를 못 읽었을 때(진짜 이례) — `routes::doctor::Outcome::Unreadable`
/// 과 같은 판단으로 레벨을 강제로 [`Level::Error`]로 올린다.
fn unreadable_report_panel(
    lang: Lang,
    problem: &route::ParseProblem,
    stderr_masked: &str,
) -> Markup {
    html! {
        (components::verdict_banner(
            Level::Error,
            lang.sel(
                "verify said it finished, but the console could not read the result.",
                "verify는 끝났다고 했지만 콘솔이 결과를 읽지 못했습니다.",
            ),
            Some(&problem.explain(lang)),
        ))
        (child_diagnostics(lang, stderr_masked))
    }
}

/// 실패류 결과(exit 1/2/3/5/시그널/미상) 패널 — 결과 문서가 없는 것이 정상이므로, 배너
/// 아래에는 원인 진단만 붙는다.
fn failure_panel(
    lang: Lang,
    level: Level,
    outcome_label: &str,
    deep_requested: bool,
    _problem: &route::ParseProblem,
    stderr_masked: &str,
) -> Markup {
    html! {
        (components::verdict_banner(level, &outcome_headline(lang, outcome_label), outcome_detail(lang, outcome_label).as_deref()))
        @if route::deep_key_missing(deep_requested, outcome_label, stderr_masked) {
            (deep_key_missing_notice(lang))
        } @else {
            (child_diagnostics(lang, stderr_masked))
        }
    }
}

/// `--deep` 키 미설정 안내 — **경로·값을 절대 싣지 않는다**(`routes::verify` 모듈 헤더
/// "`--deep` 키 미설정" 참고). 자식 stderr를 그대로 보여주는 대신 큐레이션된 문장만 낸다.
fn deep_key_missing_notice(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel(
            "Deep verification needs a decryption key",
            "심층 검증에는 복호화 키가 필요합니다",
        ),
        html! {
            p { (lang.sel(
                "This backup is encrypted, so --deep must decrypt it to check the stored data. No key was available on this host.",
                "이 백업은 암호화되어 있어 --deep이 데이터를 확인하려면 복호화해야 합니다. 이 호스트에는 그 키가 없습니다.",
            )) }
            p { (lang.sel(
                "Run this check on the host that holds the private key (age) or the symmetric key (aes-256-gcm) — see the key isolation policy (\u{a7}8.5). Structural verification (without --deep) needs no key and can run anywhere.",
                "개인키(age) 또는 대칭키(aes-256-gcm)를 보유한 호스트에서 이 검증을 실행하세요 — 키 격리 정책(\u{a7}8.5) 참고. 구조 검증(--deep 없이)은 키가 필요 없어 어디서나 실행할 수 있습니다.",
            )) }
        },
    )
}

/// 일반 실패 진단 — 마스킹을 거친 stderr 발췌(`routes::doctor::unreadable_notice`와 같은
/// 패턴).
fn child_diagnostics(lang: Lang, stderr_masked: &str) -> Markup {
    let excerpt = excerpt(stderr_masked);
    components::notice(
        Level::Error,
        lang.sel("Child diagnostics", "자식 프로세스 진단"),
        html! {
            @if excerpt.is_empty() {
                p class="muted" { (lang.sel(
                    "The child wrote nothing readable to stderr.",
                    "자식이 stderr에 읽을 수 있는 내용을 남기지 않았습니다.",
                )) }
            } @else {
                pre class="logdump" { (excerpt) }
            }
        },
    )
}

/// 결과의 한 줄 설명 — verify 문맥용(`crate::web::view::jobs::outcome_headline`과 같은
/// 이유로 이 화면이 직접 쓴다 — 모듈 헤더 참고).
fn outcome_headline(lang: Lang, label: &str) -> String {
    match label {
        "succeeded" => lang.sel("All checks passed.", "모든 검증을 통과했습니다."),
        "succeeded-with-warnings" => lang.sel(
            "Checks completed, with warnings.",
            "검증이 끝났습니다(경고 있음).",
        ),
        "lock-conflict" => lang.sel(
            "Nothing ran: another instance held things up.",
            "아무것도 실행되지 않았습니다: 다른 인스턴스가 막고 있었습니다.",
        ),
        "failed" => lang.sel("Verification failed.", "검증이 실패했습니다."),
        "rejected" => lang.sel(
            "The console sent a request the job could not accept.",
            "콘솔이 작업이 받아들일 수 없는 요청을 보냈습니다.",
        ),
        "precheck-failed" => lang.sel("Pre-flight check failed.", "사전 점검이 실패했습니다."),
        "unexpected-exit" => lang.sel(
            "Exited with a code this console does not know.",
            "이 콘솔이 모르는 종료 코드로 끝났습니다.",
        ),
        "signaled" => lang.sel("Terminated from outside.", "밖에서 종료되었습니다."),
        _ => lang.sel(
            "The result of this check is unknown.",
            "이 검증의 결과를 알 수 없습니다.",
        ),
    }
    .to_string()
}

/// 결과의 다음 행동(있으면). **exit 4의 verify/backup 구분은 여기 있다**(모듈 헤더의
/// 핵심 요구사항).
fn outcome_detail(lang: Lang, label: &str) -> Option<String> {
    let text = match label {
        "succeeded" => return None,
        "succeeded-with-warnings" => lang.sel(
            "exit 4 — success with caveats, not a failure: the verification finished and produced a report. This is NOT the backup command's exit 4 (an oplog gap promoted to a full backup) — verify's exit 4 means the checks ran to completion and found something worth flagging in THIS backup (an incomplete manifest, a broken chain link, etc). Read the warnings below; nothing here needs a retry.",
            "exit 4 — 이것은 실패가 아니라 '단서가 붙은 성공'입니다: 검증은 끝났고 보고서가 있습니다. backup 명령의 exit 4(oplog gap이 풀백업으로 승격됨)와는 뜻이 다릅니다 — verify의 exit 4는 검증 자체가 끝까지 실행됐고, 이 백업에서 표시할 문제(불완전한 manifest, 끊긴 체인 등)를 찾았다는 뜻입니다. 아래 경고를 확인하세요 — 다시 돌릴 필요는 없습니다.",
        ),
        "lock-conflict" => lang.sel(
            "exit 5 — not a failure, and nothing changed. Retry the exact same check once the other run finishes.",
            "exit 5 — 실패가 아니며 아무것도 바뀌지 않았습니다. 다른 실행이 끝난 뒤 같은 검증을 그대로 다시 시도하세요.",
        ),
        "failed" => lang.sel(
            "exit 1 — the report could not confirm this backup's integrity (a checksum mismatch, a missing/corrupt manifest, or a deep-decode failure).",
            "exit 1 — 이 백업의 무결성을 확인하지 못했습니다(체크섬 불일치, manifest 손상/부재, 또는 심층 디코드 실패).",
        ),
        "rejected" => lang.sel(
            "exit 2 — usage/configuration error. Nothing was read, so this is not a statement about the backup's integrity — only about this request.",
            "exit 2 — 사용법·설정 오류입니다. 아무것도 읽지 않았으므로 이건 백업 무결성에 대한 판정이 아니라 이 요청 자체에 대한 것입니다.",
        ),
        "precheck-failed" => lang.sel(
            "Nothing was read/decoded, so this is not a statement about the backup's integrity.",
            "아무것도 읽거나 디코드하지 않았으므로 이건 백업 무결성에 대한 판정이 아닙니다.",
        ),
        "signaled" => lang.sel(
            "The check did not decide its own outcome — it was killed before finishing.",
            "검증이 스스로 결과를 정하지 못했습니다 — 끝나기 전에 종료됐습니다.",
        ),
        _ => return None,
    };
    Some(text.to_string())
}

/// 문자열을 [`MAX_STDERR_CHARS`]로 자른다(문자 경계 — 멀티바이트 안전).
fn excerpt(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= MAX_STDERR_CHARS {
        return trimmed.to_string();
    }
    let head: String = trimmed.chars().take(MAX_STDERR_CHARS).collect();
    format!("{head}\u{2026}")
}

/// 잡 로그에서 한 스트림의 줄들을 모아 마스킹한다(방어 심층화 — 모듈 헤더).
fn collect_stream(detail: &JobDetail, stream: LogStream, registry: &SecretRegistry) -> String {
    detail
        .logs
        .iter()
        .filter(|line| line.stream == stream)
        .map(|line| registry.mask(&line.text))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::state::jobs::{JobId as StateJobId, JobLogLine, JobSummary};
    use chrono::Utc;

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    fn base_summary(args: Vec<&str>) -> JobSummary {
        JobSummary {
            id: StateJobId::generate(),
            command: "verify".to_string(),
            profile: None,
            args_masked: args.into_iter().map(str::to_string).collect(),
            started_at: Utc::now(),
            pid: None,
            finished_at: Some(Utc::now()),
            outcome: None,
            exit_code: None,
        }
    }

    fn log_line(stream: LogStream, text: &str) -> JobLogLine {
        JobLogLine {
            ts: Utc::now(),
            stream,
            text: text.to_string(),
        }
    }

    fn detail_with(summary: JobSummary, logs: Vec<JobLogLine>) -> JobDetail {
        JobDetail {
            id: summary.id,
            summary: Some(summary),
            logs,
            dropped_leading_logs: 0,
            log_file_present: true,
            unreadable_lines: 0,
        }
    }

    const SAMPLE_JSON: &str = r#"{"schema":1,"backup_id":"bk","manifest_sidecar_ok":true,"data_checksum_ok":true,"deep_decode_ok":null,"empty_slice":false,"warnings":[],"ok":true,"chain":null}"#;

    // ---- 필드가 각각 구분되어 표시된다 ----

    #[test]
    fn success_result_shows_each_field_distinctly() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("succeeded".to_string());
        let detail = detail_with(summary, vec![log_line(LogStream::Stdout, SAMPLE_JSON)]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        for expected in ["manifest_sidecar_ok", "data_checksum_ok", "deep_decode_ok"] {
            assert!(out.contains(expected), "{expected} 누락: {out}");
        }
        // 세 검사가 각각 다른 행(다른 `data-level`을 실은 `<tr>`)으로 나가야 한다 — 하나로
        // 뭉개지면 `<tr data-level="ok">`가 정확히 몇 번인지로는 구분이 안 된다. 대신 각
        // 키가 자기 행 안에 있는지 직접 확인한다.
        assert!(out.contains(r#"<td class="mono key">manifest_sidecar_ok</td>"#));
        assert!(out.contains(r#"<td class="mono key">data_checksum_ok</td>"#));
        assert!(out.contains(r#"<td class="mono key">deep_decode_ok</td>"#));
    }

    #[test]
    fn chain_fields_are_shown_when_chain_ran() {
        let doc = r#"{"schema":1,"backup_id":"i1","manifest_sidecar_ok":true,"data_checksum_ok":true,"deep_decode_ok":null,"empty_slice":false,"warnings":[],"ok":true,"chain":{"base_id":"base-1","incremental_ids":["i1"],"continuous":false,"breaks":["gap at 105.0"],"warnings":["chain warn"]}}"#;
        let mut summary = base_summary(vec!["verify", "--id", "i1", "--chain", "--json"]);
        summary.outcome = Some("succeeded-with-warnings".to_string());
        let detail = detail_with(summary, vec![log_line(LogStream::Stdout, doc)]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        assert!(out.contains("base-1"), "base_id 누락: {out}");
        assert!(out.contains("gap at 105.0"), "breaks 누락: {out}");
        assert!(out.contains("chain warn"), "체인 경고 누락: {out}");
        assert!(out.contains("PITR"), "PITR 안내 누락: {out}");
    }

    // ---- exit 4: 실패로 표시되지 않고, verify/backup 구분이 드러난다 ----

    #[test]
    fn exit_four_is_shown_as_warning_not_failure_with_verify_specific_note() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("succeeded-with-warnings".to_string());
        let doc = r#"{"schema":1,"backup_id":"bk","manifest_sidecar_ok":true,"data_checksum_ok":true,"deep_decode_ok":null,"empty_slice":false,"warnings":["incomplete manifest"],"ok":false,"chain":null}"#;
        let detail = detail_with(summary, vec![log_line(LogStream::Stdout, doc)]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        assert!(
            out.contains(r#"data-level="warn""#),
            "warn 레벨 누락: {out}"
        );
        assert!(
            !out.contains(r#"class="verdict" data-level="fail""#),
            "실패로 표시됐다: {out}"
        );
        assert!(out.contains("incomplete manifest"), "경고 문구 누락: {out}");
        assert!(
            out.contains("NOT the backup command's exit 4") || out.contains("backup"),
            "verify/backup exit4 구분 문구 누락: {out}"
        );
    }

    // ---- exit 5: 재시도 가능으로 표시된다 ----

    #[test]
    fn lock_conflict_is_shown_as_retryable() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("lock-conflict".to_string());
        let detail = detail_with(summary, vec![]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        assert!(
            !out.contains(r#"class="verdict" data-level="fail""#),
            "락 충돌이 실패로 표시됐다: {out}"
        );
        assert!(
            out.contains("Run this check again"),
            "재시도 링크 누락: {out}"
        );
    }

    #[test]
    fn only_lock_conflict_is_retryable() {
        for label in [
            "succeeded",
            "succeeded-with-warnings",
            "failed",
            "rejected",
            "precheck-failed",
            "unexpected-exit",
            "signaled",
            "unknown",
        ] {
            let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
            summary.outcome = Some(label.to_string());
            let detail = detail_with(summary, vec![]);
            let out =
                result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();
            assert!(
                !out.contains("Run this check again"),
                "{label}이 재시도 가능으로 표시됐다: {out}"
            );
        }
    }

    // ---- deep 키 미설정: 이유는 명확히, 경로·값은 노출 안 함 ----

    #[test]
    fn deep_without_key_explains_clearly_without_leaking_path_or_value() {
        const FAKE_KEY_PATH: &str = "/home/op/secret-identity-file.key";
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--deep", "--json"]);
        summary.outcome = Some("failed".to_string());
        let stderr = format!(
            "age 암호화 백업의 복호화에는 개인키가 필요합니다 — {}에 identity 파일 경로를 지정하세요(§8.5 키 격리) — verify --deep은 개인키 보유 호스트에서 실행하세요(§8.5 키 격리)",
            crate::pipeline::stage::ENV_AGE_IDENTITY_FILE
        );
        assert!(
            !stderr.contains(FAKE_KEY_PATH),
            "테스트 전제: CLI의 실제 메시지에는 경로가 없다"
        );
        let detail = detail_with(summary, vec![log_line(LogStream::Stderr, &stderr)]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        assert!(
            out.contains("Deep verification needs a decryption key"),
            "안내 문구 누락: {out}"
        );
        assert!(!out.contains(FAKE_KEY_PATH), "경로가 노출됐다: {out}");
        // 큐레이션된 안내는 원문 stderr를 그대로 반사하지 않는다 — env 변수 이름 자체도
        // 화면에 실릴 필요가 없다(운영자에게 필요한 것은 "어디서 실행하라"이지 변수 이름이
        // 아니다).
        assert!(
            !out.contains(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE),
            "원문 stderr가 그대로 노출됐다: {out}"
        );
    }

    #[test]
    fn non_deep_failure_shows_masked_stderr_excerpt() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("failed".to_string());
        let detail = detail_with(
            summary,
            vec![log_line(LogStream::Stderr, "checksum mismatch for bk")],
        );
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();
        assert!(
            out.contains("checksum mismatch for bk"),
            "진단 발췌 누락: {out}"
        );
    }

    // ---- 손상·빈·비-UTF8 stdout에서 패닉 없음 ----

    #[test]
    fn corrupt_and_empty_stdout_render_without_panicking() {
        for (outcome, stdout) in [
            ("succeeded", ""),
            ("succeeded", "not json"),
            ("succeeded", "{\"schema\":1,"),
            (
                "succeeded-with-warnings",
                r#"{"schema":99,"backup_id":"x"}"#,
            ),
        ] {
            let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
            summary.outcome = Some(outcome.to_string());
            let logs = if stdout.is_empty() {
                vec![]
            } else {
                vec![log_line(LogStream::Stdout, stdout)]
            };
            let detail = detail_with(summary, logs);
            // 패닉하지 않고 렌더되면 통과 — 문서를 못 읽었으니 오류 레벨이어야 한다.
            let out =
                result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();
            assert!(
                out.contains(r#"data-level="error""#),
                "'{outcome}'/'{stdout}' 조합이 오류로 표시되지 않았다: {out}"
            );
        }
    }

    #[test]
    fn schema_mismatch_fails_clearly() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("succeeded".to_string());
        let detail = detail_with(
            summary,
            vec![log_line(
                LogStream::Stdout,
                r#"{"schema":99,"backup_id":"x"}"#,
            )],
        );
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();
        assert!(out.contains("schema"), "스키마 불일치 안내 누락: {out}");
        assert!(out.contains("99"), "발견된 스키마 값 누락: {out}");
    }

    // ---- 진행 중 화면 ----

    #[test]
    fn running_job_shows_in_progress_and_no_report() {
        let summary = JobSummary {
            id: StateJobId::generate(),
            command: "verify".to_string(),
            profile: None,
            args_masked: vec!["verify".to_string()],
            started_at: Utc::now(),
            pid: Some(1234),
            finished_at: None,
            outcome: None,
            exit_code: None,
        };
        let detail = detail_with(summary, vec![]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();
        assert!(
            out.contains("Verification in progress"),
            "진행 중 안내 누락: {out}"
        );
        assert!(
            !out.contains("Structural checks"),
            "아직 끝나지 않았는데 결과 표가 나갔다"
        );
    }

    // ---- 마크업에 색·인라인 스타일 리터럴 없음 ----

    #[test]
    fn markup_carries_no_presentation() {
        let mut summary = base_summary(vec!["verify", "--id", "bk", "--chain", "--json"]);
        summary.outcome = Some("succeeded".to_string());
        let chain_doc = r#"{"schema":1,"backup_id":"bk","manifest_sidecar_ok":true,"data_checksum_ok":true,"deep_decode_ok":true,"empty_slice":false,"warnings":["w"],"ok":true,"chain":{"base_id":"b","incremental_ids":["i1"],"continuous":true,"breaks":[],"warnings":[]}}"#;
        let detail = detail_with(summary, vec![log_line(LogStream::Stdout, chain_doc)]);

        let rendered = [
            form_body(Lang::En, Some("bk"), "prod", None).into_string(),
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            // 자동 새로고침 스크립트에는 `#`가 없지만, 혹시라도 색 리터럴이 섞이면 여기서
            // 잡힌다. script 태그 자체는 고정 상수라 이 검사와 무관하다.
            let without_script = out.replace(AUTO_REFRESH_SCRIPT, "");
            assert!(
                !without_script.contains('#'),
                "색 리터럴로 보이는 값이 있다: {without_script}"
            );
        }
    }

    /// **보는 사람이 없으면 폴링을 멈춘다** — 잊고 열어 둔 탭이 밤새 서버를 두드리지
    /// 않게 하는 유일한 자리다(이 콘솔에는 서버측 폴링 루프가 없다).
    #[test]
    fn the_refresh_script_pauses_while_the_tab_is_hidden() {
        assert!(
            AUTO_REFRESH_SCRIPT.contains("document.hidden"),
            "가시성 검사가 없다 — 백그라운드 탭이 계속 새로고침한다"
        );
        assert!(
            AUTO_REFRESH_SCRIPT.contains("visibilitychange"),
            "다시 보일 때 재개하는 경로가 없다 — 한 번 숨으면 영영 멈춘다"
        );
        // 무조건 도는 타이머가 남아 있으면 위 두 개가 있어도 소용없다.
        assert_eq!(
            AUTO_REFRESH_SCRIPT.matches("setTimeout").count(),
            1,
            "타이머가 하나가 아니다 — 가시성 검사를 우회하는 경로가 있을 수 있다"
        );
    }

    // ---- 적대적 입력 이스케이프 ----

    #[test]
    fn hostile_strings_in_report_fields_are_escaped() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let doc = serde_json::json!({
            "schema": 1,
            "backup_id": hostile,
            "manifest_sidecar_ok": false,
            "data_checksum_ok": false,
            "deep_decode_ok": null,
            "empty_slice": false,
            "warnings": [hostile],
            "ok": false,
            "chain": {
                "base_id": hostile,
                "incremental_ids": [hostile],
                "continuous": false,
                "breaks": [hostile],
                "warnings": [hostile],
            },
        })
        .to_string();
        let mut summary = base_summary(vec!["verify", "--id", hostile, "--chain", "--json"]);
        summary.outcome = Some("succeeded-with-warnings".to_string());
        let detail = detail_with(summary, vec![log_line(LogStream::Stdout, &doc)]);
        let out =
            result_body(Lang::En, &JobId::generate(), &detail, &empty_registry()).into_string();

        assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
        assert!(!out.contains("onerror=\""), "속성이 살아 있다: {out}");
        assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
    }

    // ---- 시크릿 마스킹(방어 심층화 — 읽는 쪽이 다시 마스킹한다) ----

    #[test]
    fn secrets_left_in_logs_are_masked_by_the_view_layer() {
        const FAKE_SECRET: &str = "NOT-A-REAL-SECRET-verifyview-8b3f21";
        let mut registry = SecretRegistry::new();
        registry.register(FAKE_SECRET);

        let mut summary = base_summary(vec!["verify", "--id", "bk", "--json"]);
        summary.outcome = Some("failed".to_string());
        let detail = detail_with(
            summary,
            vec![log_line(
                LogStream::Stderr,
                &format!("auth failed with {FAKE_SECRET}"),
            )],
        );
        let out = result_body(Lang::En, &JobId::generate(), &detail, &registry).into_string();
        assert!(!out.contains(FAKE_SECRET), "시크릿 원문이 남았다: {out}");
        assert!(out.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }
}
