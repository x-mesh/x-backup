//! 재사용 컴포넌트 — **의미만 담고 모양은 담지 않는다.**
//!
//! ## 이 모듈이 있는 이유 — 구조와 스타일의 분리
//! 비주얼 아이덴티티(색·타이포·모션)는 이 브랜치 시점에 아직 확정되지 않았다(PRD Q1).
//! 그래서 앞으로 20여 개 화면이 붙는 동안 아이덴티티가 한 번 바뀌면, 마크업이 모양을
//! 알고 있는 만큼 그 변경이 화면 개수만큼 번진다. 그걸 막는 유일한 방법은 **마크업이
//! 모양을 아예 모르게** 만드는 것이다.
//!
//! 구체적 규칙 세 가지:
//!
//! 1. **상태는 `data-level` 속성으로만 표현한다.** [`Level::token`]이 내놓는
//!    `ok`/`warn`/`fail`/`error` 넷이 전부이고, 그 토큰을 색·아이콘·테두리로 바꾸는 곳은
//!    `app.css`의 `[data-level="…"]` 규칙 **한 군데뿐**이다. Rust 쪽에 `class="red"`나
//!    `style="color:…"`이 등장하는 순간 이 계약이 깨진다.
//! 2. **클래스 이름은 역할을 말한다**(`.verdict`, `.badge`, `.dtable`) — 모양을 말하지
//!    않는다(`.big-red-box` 금지). 아이덴티티가 바뀌어도 역할은 그대로다.
//! 3. **인라인 스타일·색 리터럴을 마크업에 두지 않는다.** 전부 CSS 변수를 경유한다.
//!
//! 그 결과 아이덴티티 시안 3안은 **같은 마크업에 `:root` 변수 블록만 바꿔** 렌더된다 —
//! 판독성 심사가 "마크업이 달라서 달라 보이는" 착시 없이 색·타이포만의 차이를 보게 된다.
//!
//! ## 왜 `PreEscaped`를 한 번도 쓰지 않는가
//! 이 모듈의 모든 함수는 보간값을 maud의 자동 이스케이프에 그대로 맡긴다. 여기 들어오는
//! 문자열은 전부 **자식 프로세스 stdout에서 온 남의 텍스트**(config에 사용자가 적은 경로,
//! DB가 돌려준 메시지)이고, 그중 하나라도 `PreEscaped`를 타면 그 즉시 저장형 XSS 경로가
//! 열린다. 아이콘조차 SVG 대신 CSS(`::before`)로 그리는 이유가 이것이다 — 인라인 SVG를
//! 넣으려면 `PreEscaped`가 필요해지고, 그러면 "이 모듈에는 예외가 없다"는 성질을 잃는다.

use maud::{html, Markup};

/// 화면에서 상태를 가르는 **유일한** 축.
///
/// ## `Error`가 왜 `Fail`과 따로 있는가
/// `Fail`은 "점검했고, 설정이 잘못됐다"(doctor exit 3)다. `Error`는 "점검 자체를 못
/// 했다"(자식 spawn 실패·타임아웃·출력 파싱 불가)다. 운영자가 취해야 할 행동이 정반대다 —
/// 앞은 config를 고치는 일이고, 뒤는 서버/바이너리를 의심하는 일이다. 둘을 같은 빨강으로
/// 뭉개면 "설정이 깨졌다"고 오해해 멀쩡한 config를 뒤지게 된다.
///
/// ## 왜 [`Serialize`]인가 — SSE로도 같은 토큰이 나가야 한다
/// 라이브 화면(`GET /backup/{id}`)은 초기 렌더를 서버가 하고 그 뒤 갱신을 SSE로 받는다.
/// 두 경로가 각자 레벨을 정하면 같은 잡이 새로고침 전후로 다른 색으로 보인다(실제로
/// 그랬다 — 스크립트가 종료 시 무조건 `warn`을 박아 exit 0 성공이 경고로 떴다). 그래서
/// 레벨은 **항상 서버가 정해** [`crate::web::sse::JobEvent`]에 실어 보내고, 스크립트는
/// 받은 값을 `data-level`에 반영만 한다. 직렬화 표현은 [`token`](Self::token) 하나에서만
/// 나온다(`#[serde(into = "String")]` + [`From<Level> for String`]) — 어휘가 두 벌로
/// 갈라질 여지를 타입 수준에서 없앤다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(into = "String")]
pub enum Level {
    /// 정상.
    Ok,
    /// 경고 — 동작하지만 운영자가 알아야 한다.
    Warn,
    /// 차단성 문제 — 점검 결과가 "안 된다"고 말한다.
    Fail,
    /// 점검 자체가 불가 — 결과를 얻지 못했다(위 문서 참고).
    Error,
}

impl Level {
    /// `data-level` 속성에 들어가는 의미 토큰. **CSS가 아는 유일한 어휘다.**
    ///
    /// 값을 바꾸면 `app.css`의 `[data-level="…"]` 선택자를 함께 바꿔야 한다 — 그래서
    /// 이 함수가 그 계약의 단일 지점이다.
    pub fn token(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Error => "error",
        }
    }

    /// 배지에 찍히는 짧은 라벨. 기술용어이므로 언어와 무관하게 영문 고정([`crate::i18n`] 규약).
    ///
    /// 색만으로 상태를 구분하지 않기 위해 **글자로도** 상태를 말한다 — 색각 이상 사용자와
    /// 흑백 인쇄/스크린샷에서도 판독되어야 한다(WCAG 1.4.1 "색에만 의존하지 않기").
    pub fn label(self) -> &'static str {
        match self {
            Level::Ok => "OK",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
            Level::Error => "ERROR",
        }
    }
}

/// 직렬화 표기 — [`Level::token`]과 **같은 문자열**이다([`Level`] doc의 "왜 Serialize인가"
/// 참조).
///
/// `#[serde(into = "String")]`가 요구하는 변환이고, 그 안에서 `token()`을 부르는 것이
/// 요점이다: 직렬화 어휘와 `data-level` 어휘가 한 함수에서만 나오므로 둘이 갈라질 수 없다.
impl From<Level> for String {
    fn from(level: Level) -> Self {
        level.token().to_string()
    }
}

/// 상태 배지 — 표 안이나 제목 옆에 붙는 작은 조각.
///
/// `data-level`만 실어 보내고 색은 CSS가 정한다(모듈 헤더 규칙 1).
pub fn badge(level: Level) -> Markup {
    html! {
        span class="badge" data-level=(level.token()) { (level.label()) }
    }
}

/// 화면 제목 줄. `subtitle`은 설명 문장이므로 호출자가 이미 언어를 고른 뒤 넘긴다.
pub fn page_head(title: &str, subtitle: Option<&str>) -> Markup {
    html! {
        div class="page-head" {
            h2 class="page-title" { (title) }
            @if let Some(sub) = subtitle {
                p class="page-sub" { (sub) }
            }
        }
    }
}

/// 판정 배너 — 화면 최상단에서 "지금 상태가 무엇인가"를 한 줄로 말한다.
///
/// - `headline`: 무슨 일이 일어났는지(설명 문장 — 호출자가 언어를 고른다).
/// - `detail`: 근거·다음 행동. 없으면 생략된다.
///
/// 배지·헤드라인·(선택)디테일 3층으로 두는 이유: 스캔하는 사람은 배지 색과 글자만 보고,
/// 판단해야 하는 사람은 헤드라인을 읽고, 고쳐야 하는 사람은 디테일을 읽는다. 한 줄에
/// 다 밀어넣으면 세 사람 모두에게 느려진다.
pub fn verdict_banner(level: Level, headline: &str, detail: Option<&str>) -> Markup {
    html! {
        section class="verdict" data-level=(level.token()) {
            span class="verdict__badge" { (level.label()) }
            div class="verdict__text" {
                p class="verdict__headline" { (headline) }
                @if let Some(d) = detail {
                    p class="verdict__detail" { (d) }
                }
            }
        }
    }
}

/// 라벨:값 목록(`<dl>`) — "언제·무엇을·몇 개" 같은 메타 정보를 붙인다.
///
/// `<dl>`을 쓰는 이유는 장식이 아니다 — 스크린리더가 라벨과 값의 짝을 읽어주고, 라벨 열
/// 정렬이 CSS grid 한 줄로 끝난다. `div` 두 개로 흉내내면 둘 다 잃는다.
///
/// 라벨은 영문 기술용어 고정, 값은 호출자가 만든 문자열이다.
pub fn meta_list(rows: &[(&str, String)]) -> Markup {
    html! {
        dl class="meta" {
            @for (key, value) in rows {
                dt class="meta__k" { (key) }
                dd class="meta__v" { (value) }
            }
        }
    }
}

/// 카드 한 장 — 제목 줄 + 본문. 화면을 여러 덩이로 가를 때 쓴다.
///
/// `head`를 [`Markup`]으로 받는다(문자열이 아니라) — 제목 옆에 배지·카운트가 붙는 경우가
/// 기본이고, 문자열로 받으면 그때마다 이 함수가 갈라진다.
pub fn panel(head: Markup, body: Markup) -> Markup {
    html! {
        section class="panel" {
            div class="panel__head" { (head) }
            div class="panel__body" { (body) }
        }
    }
}

/// 알림 블록 — 판정과 별개로 붙는 안내/오류 설명.
///
/// `body`는 여러 줄일 수 있으므로 문단 하나로 강제하지 않고 [`Markup`]으로 받는다.
pub fn notice(level: Level, title: &str, body: Markup) -> Markup {
    html! {
        div class="notice" data-level=(level.token()) {
            p class="notice__title" { (title) }
            div class="notice__body" { (body) }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 네 레벨의 토큰과 라벨이 서로 겹치지 않는다 — 겹치면 CSS 선택자 하나가 두 상태를
    /// 먹어 화면에서 구분이 사라진다.
    #[test]
    fn levels_have_distinct_tokens_and_labels() {
        let all = [Level::Ok, Level::Warn, Level::Fail, Level::Error];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.token(), b.token(), "토큰 충돌: {a:?} vs {b:?}");
                assert_ne!(a.label(), b.label(), "라벨 충돌: {a:?} vs {b:?}");
            }
        }
    }

    /// 배지는 `data-level` 속성으로 상태를 싣고, 글자로도 상태를 말한다(색 의존 금지).
    #[test]
    fn badge_carries_semantic_level_and_text() {
        let out = badge(Level::Warn).into_string();
        assert!(
            out.contains(r#"data-level="warn""#),
            "레벨 속성 누락: {out}"
        );
        assert!(out.contains("WARN"), "글자 라벨 누락: {out}");
    }

    /// 마크업에 색 리터럴·인라인 스타일이 새지 않는다 — 모양은 전부 CSS 몫이라는 계약을
    /// 테스트로 고정한다(모듈 헤더 규칙 1·3).
    #[test]
    fn markup_carries_no_presentation() {
        let rendered = [
            badge(Level::Fail).into_string(),
            verdict_banner(Level::Ok, "fine", Some("all good")).into_string(),
            page_head("Doctor", Some("sub")).into_string(),
            meta_list(&[("profiles", "5".to_string())]).into_string(),
            panel(html! { "head" }, html! { "body" }).into_string(),
            notice(Level::Error, "nope", html! { p { "why" } }).into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
            assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
        }
    }

    /// 컴포넌트에 들어온 적대적 문자열이 전부 이스케이프된다 — 자식 프로세스 출력이
    /// 그대로 이 자리에 들어오므로 이게 뚫리면 저장형 XSS다.
    #[test]
    fn hostile_strings_are_escaped_everywhere() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let rendered = [
            verdict_banner(Level::Fail, hostile, Some(hostile)).into_string(),
            page_head(hostile, Some(hostile)).into_string(),
            meta_list(&[("k", hostile.to_string())]).into_string(),
            notice(Level::Warn, hostile, html! { (hostile) }).into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
            assert!(!out.contains("onerror=\""), "속성이 살아 있다: {out}");
            assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
        }
    }

    /// `detail`/`subtitle`이 없으면 빈 요소를 만들지 않는다 — 빈 `<p>`가 남으면 아이덴티티
    /// 어느 안에서도 설명 없는 여백이 생긴다.
    #[test]
    fn optional_slots_are_omitted_when_absent() {
        let banner = verdict_banner(Level::Ok, "fine", None).into_string();
        assert!(
            !banner.contains("verdict__detail"),
            "빈 detail 요소가 남았다: {banner}"
        );
        let head = page_head("Doctor", None).into_string();
        assert!(
            !head.contains("page-sub"),
            "빈 subtitle 요소가 남았다: {head}"
        );
    }
}
