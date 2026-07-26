//! 공통 레이아웃 — 모든 화면이 감싸이는 껍데기(shell).
//!
//! 화면마다 `<head>`를 다시 쓰지 않게 하는 것이 이 모듈의 유일한 책임이다. 이후 태스크가
//! 추가하는 페이지는 본문 [`Markup`]만 만들어 [`shell`]에 넘긴다 — 에셋 링크·메타 태그·
//! 헤더가 한 곳에서만 바뀌게 된다.
//!
//! ## i18n 규약(R42)
//! **라벨·기술용어는 언제나 영문**이고([`crate::i18n`] 규약), 설명 문장만
//! [`Lang::sel`](crate::i18n::Lang::sel)로 ko/en을 고른다. `<html lang>` 속성만은 설명
//! 언어를 따라간다 — 스크린리더 발음과 브라우저 번역 판단에 쓰이기 때문이다.
//!
//! ## 함정
//! - `<title>`은 브라우저 이력·탭에 남으므로 프로파일명 같은 식별자를 넣지 않는다.
//! - htmx 스크립트 태그는 아직 넣지 않는다. 실제로 부분 갱신을 쓰는 화면(t15~)이 생길 때
//!   벤더 JS를 함께 임베드해야 하고, 쓰지 않는 60KB를 미리 박을 이유가 없다.

use maud::{html, Markup, DOCTYPE};

use crate::i18n::Lang;

/// 헤더에 노출하는 제품명. 라벨이므로 언어와 무관하게 영문 고정.
const PRODUCT: &str = "x-backup";

/// 상단 내비게이션 항목 — `(라벨, 경로)`. 라벨은 영문 고정([`crate::i18n`] 규약).
///
/// 화면이 늘어나면 여기 한 줄씩 추가한다. 링크가 코드에 있고 경로가 상수를 가리키므로,
/// 라우트를 옮기면 컴파일이 깨져 죽은 링크가 남지 않는다.
///
/// 라벨도 각 화면의 `*_TITLE` 상수를 그대로 쓴다 — 라벨과 `<title>`이 같아야 한다는 규약
/// ([`nav_labels_match_titles`])을 문자열을 두 번 적는 대신 **같은 상수를 가리켜서** 지킨다.
///
/// 순서는 운영 흐름이다: 개요(Console·Dashboard·Monitor·Catalog) → 실행(Backup·Verify·Peek·
/// Restore·Prune·Migrate·Schedule) → 사후(Jobs·Lock) → 설정(Config·Doctor).
fn nav_items() -> [(&'static str, &'static str); 15] {
    use crate::web::routes;
    [
        ("Console", "/"),
        (
            routes::dashboard::DASHBOARD_TITLE,
            routes::dashboard::DASHBOARD_PATH,
        ),
        (
            routes::monitor::MONITOR_TITLE,
            routes::monitor::MONITOR_PATH,
        ),
        (
            routes::catalog::CATALOG_TITLE,
            routes::catalog::CATALOG_PATH,
        ),
        (routes::backup::BACKUP_TITLE, routes::backup::BACKUP_PATH),
        (routes::verify::VERIFY_TITLE, routes::verify::VERIFY_PATH),
        (routes::peek::PEEK_TITLE, routes::peek::PEEK_PATH),
        (
            routes::restore::RESTORE_TITLE,
            routes::restore::RESTORE_PATH,
        ),
        (routes::prune::PRUNE_TITLE, routes::prune::PRUNE_PATH),
        (
            routes::migrate::MIGRATE_TITLE,
            routes::migrate::MIGRATE_PATH,
        ),
        (
            routes::schedule::SCHEDULE_TITLE,
            routes::schedule::SCHEDULE_PATH,
        ),
        (routes::jobs::JOBS_TITLE, routes::jobs::JOBS_PATH),
        (routes::lock::LOCK_TITLE, routes::lock::LOCK_PATH),
        (routes::config::CONFIG_TITLE, routes::config::CONFIG_PATH),
        (routes::doctor::DOCTOR_TITLE, routes::doctor::DOCTOR_PATH),
    ]
}

/// 모든 화면의 공통 껍데기.
///
/// - `title`: `<title>`에 들어가는 화면 이름(영문 라벨 — "Console", "Catalog" 등).
/// - `body`: 본문 마크업. 호출자가 `html! { ... }`로 만들어 넘긴다.
///
/// ## 현재 화면 표시를 `title`로 판정하는 이유
/// `title`이 곧 그 화면의 내비게이션 라벨이라는 규약을 둔다([`nav_items`]의 첫 원소와
/// 일치). 그래서 `shell`에 "현재 경로" 인자를 따로 받지 않아도 `aria-current`를 붙일 수
/// 있다 — 인자를 하나 더 받으면 모든 화면이 자기 경로를 두 번(라우터에 한 번, 여기에 한 번)
/// 적어야 하고 그 둘이 어긋나는 사고가 생긴다. 규약이 깨지면 강조가 사라질 뿐 화면은
/// 정상이므로(fail-soft), [`nav_labels_match_titles`] 테스트로 드리프트만 잡아 둔다.
pub fn shell(lang: Lang, title: &str, body: Markup) -> Markup {
    let lang_attr = lang.sel("en", "ko");
    html! {
        (DOCTYPE)
        html lang=(lang_attr) {
            head {
                meta charset="utf-8";
                // 운영 콘솔은 노트북·태블릿에서 함께 열린다 — 초기 축소 렌더를 막는다.
                meta name="viewport" content="width=device-width, initial-scale=1";
                // 화면 어디에도 외부 리소스를 두지 않으므로 referrer를 흘릴 이유가 없다.
                meta name="referrer" content="no-referrer";
                title { (PRODUCT) " — " (title) }
                link rel="stylesheet" href=(super::app_css_url());
            }
            body {
                div class="shell" {
                    header class="shell-head" {
                        h1 { (PRODUCT) }
                        span class="ver" { "v" (env!("CARGO_PKG_VERSION")) }
                        nav class="shell-nav" aria-label="Screens" {
                            @for (label, href) in nav_items() {
                                @if label == title {
                                    // 현재 화면은 링크로 두되 aria-current로 표시한다 —
                                    // 링크를 지우면 키보드 탐색 순서가 화면마다 달라진다.
                                    a href=(href) aria-current="page" { (label) }
                                } @else {
                                    a href=(href) { (label) }
                                }
                            }
                        }
                    }
                    main { (body) }
                }
            }
        }
    }
}

/// 최초 진입 화면 — 서버가 살아 있다는 사실만 알린다.
///
/// 대시보드(t15)가 이 자리를 차지하면 그때 교체된다. 지금 이 페이지가 존재하는 이유는
/// [`shell`]이 실제로 렌더되는 경로를 하나 두어, 레이아웃이 쓰이지 않는 코드로 남지 않게
/// 하려는 것이다. 서버 내부 정보(bind 주소·config 경로)는 의도적으로 싣지 않는다.
pub fn landing(lang: Lang) -> Markup {
    html! {
        div class="card" {
            p {
                (lang.sel(
                    "The web console is running. Operational screens land in later steps.",
                    "웹 콘솔이 동작 중입니다. 운영 화면은 후속 단계에서 붙습니다.",
                ))
            }
            p class="muted" {
                (lang.sel("Health check: ", "헬스 체크: "))
                code { a href=(crate::web::server::HEALTHZ_PATH) { (crate::web::server::HEALTHZ_PATH) } }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 껍데기는 문서 골격 + 지문 붙은 스타일시트 링크를 포함한다.
    #[test]
    fn shell_renders_document_skeleton() {
        let out = shell(Lang::En, "Console", html! { p { "hi" } }).into_string();
        assert!(out.starts_with("<!DOCTYPE html>"), "DOCTYPE 누락: {out}");
        assert!(out.contains(r#"<html lang="en">"#), "lang 속성 누락");
        assert!(out.contains("<title>x-backup — Console</title>"));
        assert!(
            out.contains(&format!(r#"href="{}""#, super::super::app_css_url())),
            "지문 붙은 CSS 링크 누락: {out}"
        );
        assert!(out.contains("<p>hi</p>"), "본문 슬롯이 렌더되지 않음");
    }

    /// `--lang ko`는 `<html lang>`과 설명 문장에만 반영되고 라벨(제품명)은 영문 고정이다.
    #[test]
    fn korean_switches_descriptions_only() {
        let out = shell(Lang::Ko, "Console", landing(Lang::Ko)).into_string();
        assert!(out.contains(r#"<html lang="ko">"#));
        assert!(out.contains("웹 콘솔이 동작 중입니다"), "한국어 설명 누락");
        assert!(out.contains("<h1>x-backup</h1>"), "제품명 라벨은 영문 고정");
        assert!(
            out.contains("<title>x-backup — Console</title>"),
            "라벨 번역 금지"
        );
    }

    /// 내비게이션은 등록된 모든 화면으로 가는 링크를 낸다 — 죽은 링크가 없는지, 그리고
    /// 현재 화면이 하나만 강조되는지 확인한다.
    #[test]
    fn nav_lists_every_screen_and_marks_current() {
        let out = shell(Lang::En, "Doctor", html! {}).into_string();
        for (label, href) in nav_items() {
            assert!(
                out.contains(&format!(r#"href="{href}""#)),
                "{label} 링크 누락: {out}"
            );
        }
        assert_eq!(
            out.matches(r#"aria-current="page""#).count(),
            1,
            "현재 화면 표시가 0개 또는 2개 이상이다: {out}"
        );
        assert!(
            out.contains(&format!(
                r#"href="{}" aria-current="page""#,
                crate::web::routes::doctor::DOCTOR_PATH
            )),
            "Doctor가 현재 화면으로 표시되지 않았다: {out}"
        );
    }

    /// 어떤 화면에도 속하지 않는 `title`이 오면 강조가 사라질 뿐 링크는 온전하다(fail-soft).
    ///
    /// 예전에는 아직 없던 화면 이름("Catalog")을 견본으로 썼는데, 그 화면이 실제로 생기자
    /// 테스트의 전제가 조용히 무너졌다. 그래서 지금은 **견본이 내비에 없다는 사실 자체를
    /// 먼저 단정**한다 — 화면이 늘어도 이 테스트가 의미를 잃지 않는다.
    #[test]
    fn unknown_title_marks_nothing_current() {
        const NOT_A_SCREEN: &str = "Not A Screen";
        assert!(
            !nav_items().iter().any(|(label, _)| *label == NOT_A_SCREEN),
            "견본 title이 실제 화면이 됐다 — 다른 이름을 골라야 한다"
        );

        let out = shell(Lang::En, NOT_A_SCREEN, html! {}).into_string();
        assert!(
            !out.contains("aria-current"),
            "규약을 벗어난 title이 엉뚱한 항목을 강조했다: {out}"
        );
        assert!(out.contains(r#"href="/""#), "링크 자체는 남아야 한다");
    }

    /// 내비게이션 라벨은 화면이 `shell`에 넘기는 `title`과 같아야 한다 — 이 규약이 깨지면
    /// 현재 화면 강조가 조용히 사라진다(모듈 `shell` 문서 참고).
    #[test]
    fn nav_labels_match_titles() {
        let labels: Vec<&str> = nav_items().iter().map(|(l, _)| *l).collect();
        assert!(labels.contains(&"Console"), "index 화면 라벨 드리프트");
        assert!(
            labels.contains(&crate::web::routes::doctor::DOCTOR_TITLE),
            "doctor 화면 라벨 드리프트: {labels:?}"
        );
    }

    /// maud 자동 이스케이프에 의존한다 — 본문에 섞인 태그가 실행되지 않아야 한다(R15 XSS).
    #[test]
    fn interpolated_text_is_escaped() {
        let hostile = "<script>alert(1)</script>";
        let out = shell(Lang::En, "Console", html! { p { (hostile) } }).into_string();
        assert!(
            !out.contains("<script>alert"),
            "이스케이프되지 않았다: {out}"
        );
        assert!(
            out.contains("&lt;script&gt;"),
            "이스케이프 형태가 아니다: {out}"
        );
    }
}
