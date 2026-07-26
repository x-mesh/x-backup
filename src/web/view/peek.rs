//! `peek` 화면의 순수 렌더 계층 — 이미 파싱된 데이터를 마크업으로 바꾸기만 한다.
//!
//! ## 이 화면이 다른 화면과 다른 이유
//! 다른 화면(`doctor`·`backup`·`config`)이 그리는 것은 전부 **메타데이터**다 — 백업
//! id·크기·상태·설정값. 이 화면이 그리는 것은 **남의 DB에 들어 있는 실제 문서/행**이다.
//! 그래서 이 파일의 모든 함수는 "이 문자열이 PII일 수 있다"는 전제를 깔고 설계됐다.
//!
//! ## 기본이 마스킹인 이유
//! 브라우저 화면에 값을 한 번 그리는 순간 그 값은 스크린샷·개발자도구 히스토리·
//! 프록시 접근 로그·화면 공유로 번질 수 있는 표면을 새로 얻는다. 반면 "이 컬렉션에
//! `email` 필드가 있고 문자열 42자짜리 값이 들어 있다"는 사실은 스키마 정보이지
//! 개인정보 자체가 아니다. 그래서 기본 렌더는 **타입과 길이만** 보여주고([`shape_markup`]),
//! 원문은 [`crate::web::routes::peek`]가 감사 기록을 남긴 뒤에만 [`revealed_markup`]으로
//! 넘어간다 — "보여주지 않는 것"이 1차 방어이고, 그 다음이 감사 기록이라는 2차 방어다.
//!
//! ## 객체 키는 지우지 않는다 — SSE 마스킹과의 의도적인 차이
//! `fix-jobs-high`가 고친 [`crate::web::sse`]의 `mask_json`은 `JobEvent::Done.summary`의
//! 객체 키까지 [`SecretRegistry::mask`]에 통과시킨다. 그 파일의 근거는 "그 값이 어느
//! 서브커맨드의 출력인지 SSE 레이어가 모른다"는 것이다 — `peek --json`이 섞여 들어올 수
//! 있고, 그때 키는 남의 DB 필드명이므로 값과 다르지 않다는 논리다.
//!
//! **이 화면은 그 전제가 성립하지 않는다.** 이 화면은 항상 `peek --json`의 출력만
//! 다루고, 그 스키마를 안다. 그리고 이 화면의 존재 이유 자체가 "이 컬렉션에 어떤 필드가
//! 있는가"를 보여주는 것이다 — 키를 지우면 화면이 스스로의 목적을 잃는다(빈 상자만
//! 남는다). 그래서 이 화면은 값은 기본적으로 감추되 **키는 항상 보여준다.**
//!
//! 그렇다고 키를 무조건 안전하다고 가정하지는 않는다. `SecretRegistry::mask`는 "값
//! 자체가 등록된 시크릿과 정확히 일치하는가"만 검사하는 함수이지 "키라는 이유로 지운다"는
//! 함수가 아니다([`crate::web::mask`] 헤더). 그래서 [`revealed_markup`]은 키에도 같은
//! 레지스트리를 통과시킨다 — 등록된 시크릿(URI 비밀번호 등)이 우연히 필드 이름 자리에
//! 나타나는 병적인 경우까지 잡기 위해서다. 이것은 SSE와 같은 방어선(레지스트리 마스킹)을
//! 같은 강도로 유지하는 것이고, "키를 지운다"는 SSE의 **정책**과는 다른 결정이다 — 정책이
//! 다른 이유는 위에서 설명한 스키마 인지 여부의 차이다.
//!
//! ## 병리적 입력 방어 — 값과 근거
//! 이 화면이 그리는 문서는 신뢰할 수 없는 두 경로에서 온다: (1) 실제 운영 DB에 쌓인
//! 데이터는 그 자체로 임의의 크기·구조일 수 있고, (2) DB 쓰기 권한을 가진 내부자가
//! 의도적으로 병적인 문서를 심어 이 콘솔을 노릴 수도 있다. 그래서 렌더 단계에서 아래
//! 상한을 전부 강제한다. **상한에 걸렸다는 사실은 항상 화면에 `peek-trunc` 요소로
//! 남는다** — 조용한 절단은 그 자체로 거짓말이다(데이터가 실제보다 적어 보이면
//! 운영자가 "문서가 몇 개 없네"라고 잘못 판단할 수 있다).
//!
//! | 상한 | 값 | 근거 |
//! |---|---|---|
//! | [`MAX_RENDER_DEPTH`] | 10 | 중첩 객체/배열을 재귀로 그릴 때 **깊이를 먼저 확인하고 나서 재귀한다** — 그래서 실제 문서가 얼마나 깊든 이 함수의 호출 스택은 상수 깊이를 넘지 않는다. 사람이 화면에서 눈으로 따라갈 수 있는 중첩도 이 언저리다. (파싱 단계의 깊이 상한은 이 값과 무관하다 — 그쪽은 `serde_json`의 재귀 상한이 강제하고, `crate::web::jsonguard`는 그 거부에 실제 깊이를 붙여 진단을 낫게 할 뿐이다. 이 값은 렌더 가독성 상한이다.) |
//! | [`MAX_FIELDS_SHOWN`] | 200 | 필드 수천 개짜리 문서(예: 동적 스키마 컬렉션)가 화면을 무한정 늘리지 않게 한다. |
//! | [`MAX_ARRAY_ITEMS_SHOWN`] | 50 | 배열 하나가 수만 개 원소를 담고 있어도 화면은 유한하다. |
//! | [`MAX_FIELD_CHARS`] | 2000 | 원문 노출 시 1MB짜리 단일 문자열 필드가 화면을 통째로 채우는 것을 막는다(마스킹 모드는 애초에 값을 그리지 않으므로 이 상한과 무관하다 — 길이 계산은 하되 내용은 만지지 않는다). |
//! | [`MAX_NAMESPACES_SHOWN`] | 200 | 네임스페이스 개요 목록도 같은 이유로 상한을 둔다. |
//!
//! ## 제어문자·비-UTF8
//! 자식 stdout은 [`crate::web::job::JobCompletion::stdout`] 단계에서 이미 lossy
//! UTF-8 변환을 거치므로(`String::from_utf8_lossy`) 이 파일에 도달하는 값은 항상 유효한
//! `String`이다 — 다만 원래 바이트가 깨져 있었다면 U+FFFD로 치환된 채로 온다. 이 파일은
//! 그 값을 그대로 받아 렌더한다(패닉하지 않는다는 것만 보장하면 된다).
//!
//! 제어문자(개행·탭 포함, `char::is_control`)는 화면 표를 한 줄로 유지하기 위해
//! [`strip_control_chars`]가 제거한다 — HTML 태그 자체는 이 파일이 손대지 않는다,
//! maud의 자동 이스케이프가 이미 `<`/`>`/`&`를 실체 참조로 바꾸므로 `PreEscaped`를 한 번도
//! 쓰지 않는 이 파일의 모든 함수가 저절로 안전하다([`components`] 헤더).
//!
//! ## 마크업 규약
//! 다른 화면과 마찬가지로 색·인라인 스타일 리터럴을 두지 않는다. 상태는
//! [`components::Level`]의 `data-level` 토큰만 쓴다. 이 화면 고유의 개념(값 모양·문서
//! 카드·절단 표시)에는 새 클래스 이름(`peek-*`)을 붙이되 `app.css`는 건드리지 않는다 —
//! 비주얼 아이덴티티가 아직 확정되지 않은 시점에 이 태스크 하나가 스타일시트까지
//! 고치면 동시 작업 중인 6개 화면과 충돌한다. 스타일이 없어도 구조는 완전하다.

use maud::{html, Markup};
use serde_json::Value;

use crate::i18n::Lang;
use crate::web::job::JobOutcome;
use crate::web::mask::SecretRegistry;
use crate::web::routes::peek::PEEK_PATH;
use crate::web::view::components::{self, Level};

// ---------------------------------------------------------------------------
// 렌더 상한 — 근거는 모듈 헤더 표 참조
// ---------------------------------------------------------------------------

/// 값 트리를 재귀로 그릴 때의 최대 깊이. 이 깊이를 넘는 자리는 더 내려가지 않고
/// [`truncation_marker`]로 멈춘다 — 그래서 실제 문서가 얼마나 깊어도 재귀 호출 횟수는
/// 이 값을 넘지 않는다(스택 안전).
pub const MAX_RENDER_DEPTH: usize = 10;

/// 객체 한 개에서 보여주는 최대 필드 수.
pub const MAX_FIELDS_SHOWN: usize = 200;

/// 배열 한 개에서 보여주는 최대 원소 수.
pub const MAX_ARRAY_ITEMS_SHOWN: usize = 50;

/// 원문 노출 시 문자열 값 하나를 보여주는 최대 문자 수.
pub const MAX_FIELD_CHARS: usize = 2000;

/// 네임스페이스 개요 목록에서 보여주는 최대 행 수.
pub const MAX_NAMESPACES_SHOWN: usize = 200;

// ---------------------------------------------------------------------------
// 데이터 모델 — routes::peek이 자식 stdout을 파싱해 넘기는 형태
// ---------------------------------------------------------------------------

/// 네임스페이스 개요 한 줄(`--ns` 없는 `peek --json`의 `namespaces[]` 원소).
#[derive(Debug, Clone)]
pub struct NamespaceSummary {
    /// `db.collection` 또는 `schema.table`.
    pub ns: String,
    /// 문서/행 수 — 엔진에 따라 정수형이 다를 수 있어 원본 JSON 값을 그대로 들고 다닌다.
    pub count: Value,
    /// 이 네임스페이스의 최신 1건(없으면 `None` — 빈 컬렉션).
    pub latest: Option<Value>,
}

// ---------------------------------------------------------------------------
// 값 트리 렌더 — 마스킹(기본) / 원문(명시 노출) 두 갈래
// ---------------------------------------------------------------------------

/// 절단 표시 하나. **조용한 절단은 거짓말이다**(모듈 헤더) — 상한에 걸릴 때마다 이
/// 함수를 거쳐 눈에 보이는 요소를 남긴다. 문구 언어 선택은 호출부가 이미 끝낸 뒤 넘긴다.
fn truncation_marker(text: &str) -> Markup {
    html! { span class="peek-trunc" data-level="warn" { (text) } }
}

/// 깊이 상한에 걸렸을 때의 표시.
fn depth_limit_marker(lang: Lang) -> Markup {
    truncation_marker(lang.sel("…(max nesting depth reached)", "…(최대 중첩 깊이 도달)"))
}

/// MongoDB 확장 JSON(relaxed extJSON) 한 겹 래퍼를 사람이 읽는 타입 이름으로 접는다.
///
/// `bson::Bson::into_relaxed_extjson()`(`src/cli/handlers/peek.rs`)이 `ObjectId`·`Date`·
/// `Decimal128` 같은 BSON 전용 타입을 `{"$oid": "…"}` 같은 한-키 객체로 표현한다. 이걸
/// 그냥 "객체"로 뭉개면 마스킹 화면에서 `_id` 필드가 전부 "object{1}"로 보여 아무 정보가
/// 없다. `$`로 시작하는 키 하나짜리 객체를 발견하면 그 타입 이름을 대신 보여준다 — 값이
/// 아니라 **타입**만 새는 것이므로 마스킹 원칙을 어기지 않는다. 매핑에 없는 `$`-키는
/// "bson-tagged"로 떨어뜨린다(모르는 타입을 조용히 "object"로 위장하지 않는다).
fn bson_wrapper_type(map: &serde_json::Map<String, Value>) -> Option<&'static str> {
    if map.len() != 1 {
        return None;
    }
    let key = map.keys().next()?;
    match key.as_str() {
        "$oid" => Some("objectid"),
        "$date" => Some("date"),
        "$numberLong" | "$numberInt" | "$numberDecimal" | "$numberDouble" => Some("number"),
        "$binary" => Some("binary"),
        "$regularExpression" => Some("regex"),
        "$timestamp" => Some("timestamp"),
        "$minKey" | "$maxKey" => Some("key-bound"),
        _ if key.starts_with('$') => Some("bson-tagged"),
        _ => None,
    }
}

/// **마스킹 렌더** — 값 대신 타입·길이만 보여준다. 키는 항상 보인다(모듈 헤더).
///
/// `depth`는 호출자가 0에서 시작해 내려갈 때마다 1을 더한다. [`MAX_RENDER_DEPTH`]를
/// 넘으면 자식을 더 들여다보지 않고 즉시 반환한다 — 그래서 이 함수의 재귀 깊이는
/// 문서의 실제 깊이와 무관하게 상수로 묶인다.
pub fn shape_markup(lang: Lang, value: &Value, depth: usize) -> Markup {
    if depth > MAX_RENDER_DEPTH {
        return depth_limit_marker(lang);
    }
    match value {
        Value::Null => html! { span class="peek-shape muted" { "null" } },
        Value::Bool(_) => html! { span class="peek-shape" { "boolean" } },
        Value::Number(_) => html! { span class="peek-shape" { "number" } },
        Value::String(s) => {
            let len = s.chars().count();
            html! { span class="peek-shape" { "string(" (len) ")" } }
        }
        Value::Array(items) => {
            html! { span class="peek-shape" { "array[" (items.len()) "]" } }
        }
        Value::Object(map) => {
            if let Some(kind) = bson_wrapper_type(map) {
                return html! { span class="peek-shape" { (kind) } };
            }
            let shown: Vec<_> = map.iter().take(MAX_FIELDS_SHOWN).collect();
            let hidden = map.len().saturating_sub(shown.len());
            html! {
                dl class="peek-shape-obj" {
                    @for (key, val) in &shown {
                        dt class="mono" { (key) }
                        dd { (shape_markup(lang, val, depth + 1)) }
                    }
                }
                @if hidden > 0 {
                    (truncation_marker(lang.sel(
                        &format!("…{hidden} more fields"),
                        &format!("…필드 {hidden}개 더"),
                    )))
                }
            }
        }
    }
}

/// 제어문자를 제거한다(줄바꿈·탭 포함 — 표 한 줄을 유지하기 위해서다). 개행조차 유지하지
/// 않는 이유: 이 값은 표/카드 안의 한 줄짜리 셀에 들어가고, 여러 줄 문자열을 그대로
/// 두면 레이아웃이 깨진다. 무엇이 제거됐는지는 반환값의 두 번째 원소로 알린다 — 조용한
/// 변형도 조용한 절단만큼 오해를 부른다.
fn strip_control_chars(input: &str) -> (String, bool) {
    let mut out = String::with_capacity(input.len());
    let mut removed = false;
    for c in input.chars() {
        if c.is_control() {
            removed = true;
        } else {
            out.push(c);
        }
    }
    (out, removed)
}

/// 문자 수 기준으로 자른다(문자 경계 보존 — 멀티바이트 중간에서 자르지 않는다).
fn truncate_chars(input: &str, max_chars: usize) -> (String, bool) {
    if input.chars().count() <= max_chars {
        return (input.to_string(), false);
    }
    (input.chars().take(max_chars).collect(), true)
}

/// **원문 렌더** — 실제 값을 보여준다. `routes::peek`가 감사 기록을 남긴 뒤에만 호출해야
/// 한다(이 함수 자신은 그 규율을 강제하지 못한다 — 호출 시점 판단은 호출부 책임이다).
///
/// `registry`는 시크릿 원문이 남의 DB 필드 값 자리에 우연히 나타나는 병적인 경우까지
/// 지운다(모듈 헤더 "객체 키는 지우지 않는다" 절). 키에도 같은 레지스트리를 통과시킨다.
pub fn revealed_markup(
    lang: Lang,
    value: &Value,
    depth: usize,
    registry: &SecretRegistry,
) -> Markup {
    if depth > MAX_RENDER_DEPTH {
        return depth_limit_marker(lang);
    }
    match value {
        Value::Null => html! { span class="peek-val muted" { "null" } },
        Value::Bool(b) => html! { span class="peek-val mono" { (b.to_string()) } },
        Value::Number(n) => html! { span class="peek-val mono" { (n.to_string()) } },
        Value::String(s) => {
            let masked = registry.mask(s);
            let (sanitized, ctrl_removed) = strip_control_chars(&masked);
            let (shown, char_truncated) = truncate_chars(&sanitized, MAX_FIELD_CHARS);
            html! {
                span class="peek-val" { (shown) }
                @if char_truncated {
                    (truncation_marker(lang.sel("…(truncated)", "…(잘림)")))
                }
                @if ctrl_removed {
                    span class="peek-note muted" {
                        (lang.sel(
                            "(control characters removed)",
                            "(제어문자 제거됨)",
                        ))
                    }
                }
            }
        }
        Value::Array(items) => {
            let shown: Vec<_> = items.iter().take(MAX_ARRAY_ITEMS_SHOWN).collect();
            let hidden = items.len().saturating_sub(shown.len());
            html! {
                ul class="peek-array" {
                    @for item in &shown {
                        li { (revealed_markup(lang, item, depth + 1, registry)) }
                    }
                }
                @if hidden > 0 {
                    (truncation_marker(lang.sel(
                        &format!("…{hidden} more items"),
                        &format!("…원소 {hidden}개 더"),
                    )))
                }
            }
        }
        Value::Object(map) => {
            let shown: Vec<_> = map.iter().take(MAX_FIELDS_SHOWN).collect();
            let hidden = map.len().saturating_sub(shown.len());
            html! {
                dl class="peek-doc" {
                    @for (key, val) in &shown {
                        dt class="mono" { (registry.mask(key)) }
                        dd { (revealed_markup(lang, val, depth + 1, registry)) }
                    }
                }
                @if hidden > 0 {
                    (truncation_marker(lang.sel(
                        &format!("…{hidden} more fields"),
                        &format!("…필드 {hidden}개 더"),
                    )))
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 페이지 조립
// ---------------------------------------------------------------------------

/// 프로파일을 아직 고르지 않았을 때의 선택 폼. GET이다 — 프로파일/네임스페이스 탐색은
/// 안전한(부작용 없는) 내비게이션이므로 북마크·새로고침이 자유로워야 한다(원문 노출
/// 토글만 POST — `routes::peek` 모듈 헤더 참조).
pub fn picker_body(lang: Lang, profiles: &[String], error: Option<Markup>) -> Markup {
    html! {
        (components::page_head(
            "Peek",
            Some(lang.sel(
                "Look at real documents in a collection — masked by default.",
                "컬렉션의 실제 문서를 들여다봅니다 — 기본은 마스킹됩니다.",
            )),
        ))
        @if let Some(err) = error {
            (err)
        }
        @if profiles.is_empty() {
            div class="card" {
                p { (lang.sel(
                    "No profiles were found in the config file.",
                    "config 파일에서 프로파일을 찾지 못했습니다.",
                )) }
            }
        } @else {
            (components::panel(
                html! { h3 class="panel__title" { "Choose a profile" } },
                html! {
                    form method="get" action=(PEEK_PATH) {
                        div class="field" {
                            label class="field__label mono" for="f-profile" { "profile" }
                            select id="f-profile" name="profile" required {
                                @for profile in profiles {
                                    option value=(profile) { (profile) }
                                }
                            }
                        }
                        div class="actions" {
                            button type="submit" { (lang.sel("Browse", "둘러보기")) }
                        }
                    }
                },
            ))
        }
    }
}

/// 문서/행 원소 하나를 카드로 감싼다(순번 라벨 포함 — 여러 건을 위아래로 훑을 때 위치를
/// 잃지 않게 한다).
fn document_card(index: usize, body: Markup) -> Markup {
    html! {
        div class="peek-doc-card" {
            p class="peek-doc-index mono muted" { "#" (index) }
            (body)
        }
    }
}

/// 네임스페이스 개요(`GET /peek?profile=…`, `ns` 없음) — 컬렉션/테이블별 문서 수 +
/// 최신 1건(마스킹). 여기서는 원문을 절대 보여주지 않는다 — 원문 노출은 특정
/// 네임스페이스 하나를 고른 뒤에만 가능하다(`routes::peek` 헤더 "왜 개요에서는 노출을
/// 허용하지 않는가").
pub fn overview_body(
    lang: Lang,
    profile: &str,
    items: &[NamespaceSummary],
    ns_href: impl Fn(&str) -> String,
) -> Markup {
    let shown: Vec<_> = items.iter().take(MAX_NAMESPACES_SHOWN).collect();
    let hidden = items.len().saturating_sub(shown.len());
    let subtitle = format!("{} — {profile}", lang.sel("Namespaces", "네임스페이스"));
    html! {
        (components::page_head("Peek", Some(&subtitle)))
        @if shown.is_empty() {
            div class="card" {
                p class="muted" { (lang.sel("(no user data)", "(사용자 데이터 없음)")) }
            }
        } @else {
            div class="dtable-scroll" {
                table class="dtable" {
                    thead {
                        tr {
                            th scope="col" { "Namespace" }
                            th scope="col" { "Count" }
                            th scope="col" { "Latest (masked)" }
                        }
                    }
                    tbody {
                        @for item in &shown {
                            tr {
                                td class="mono key" {
                                    a href=(ns_href(&item.ns)) { (item.ns) }
                                }
                                td class="mono num" { (item.count.to_string()) }
                                td {
                                    @match &item.latest {
                                        Some(doc) => (shape_markup(lang, doc, 0)),
                                        None => span class="muted" { (lang.sel("(empty)", "(비어 있음)")) },
                                    }
                                }
                            }
                        }
                    }
                }
            }
            @if hidden > 0 {
                (truncation_marker(lang.sel(
                    &format!("…{hidden} more namespaces"),
                    &format!("…네임스페이스 {hidden}개 더"),
                )))
            }
        }
    }
}

/// 네임스페이스 상세(`GET /peek?profile=…&ns=…`) — 최신 N건. `revealed`가 `false`면
/// [`shape_markup`], `true`면 [`revealed_markup`]으로 그린다. `revealed=true`는 오직
/// `routes::peek::reveal`(POST, 감사 기록을 마친 뒤)에서만 호출되어야 한다.
#[allow(clippy::too_many_arguments)]
pub fn namespace_body(
    lang: Lang,
    profile: &str,
    ns: &str,
    limit: u32,
    items: &[Value],
    revealed: bool,
    registry: &SecretRegistry,
    reveal_action: &str,
    masked_href: &str,
) -> Markup {
    let subtitle = format!("{profile} / {ns}");
    html! {
        (components::page_head("Peek", Some(&subtitle)))
        @if revealed {
            (components::notice(Level::Warn,
                lang.sel("Raw values are showing", "원문이 표시 중입니다"),
                html! {
                    p { (lang.sel(
                        "This view was recorded in the audit log and is never cached (Cache-Control: no-store). Reload to return to the masked view.",
                        "이 화면은 감사 로그에 기록됐고 절대 캐시되지 않습니다(Cache-Control: no-store). 새로고침하면 마스킹된 화면으로 돌아갑니다.",
                    )) }
                    p { a href=(masked_href) { (lang.sel("Back to masked view", "마스킹된 화면으로")) } }
                },
            ))
        } @else {
            (components::panel(
                html! { h3 class="panel__title" { (lang.sel("Reveal raw values", "원문 노출")) } },
                html! {
                    p class="field__hint" { (lang.sel(
                        "This shows real document contents and is recorded in the audit log. It is not undoable — anyone who sees your screen sees the raw data.",
                        "실제 문서 내용을 보여주며 감사 로그에 기록됩니다. 되돌릴 수 없습니다 — 화면을 보는 사람은 원문 데이터를 함께 봅니다.",
                    )) }
                    form method="post" action=(reveal_action) {
                        input type="hidden" name="profile" value=(profile);
                        input type="hidden" name="ns" value=(ns);
                        input type="hidden" name="limit" value=(limit.to_string());
                        div class="actions" {
                            button type="submit" { (lang.sel("Reveal", "원문 보기")) }
                        }
                    }
                },
            ))
        }
        @if items.is_empty() {
            div class="card" {
                p class="muted" { (lang.sel("(empty)", "(비어 있음)")) }
            }
        } @else {
            div class="peek-doclist" {
                @for (i, item) in items.iter().enumerate() {
                    (document_card(i + 1, if revealed {
                        revealed_markup(lang, item, 0, registry)
                    } else {
                        shape_markup(lang, item, 0)
                    }))
                }
            }
        }
    }
}

/// 잡 종료 코드를 화면 레벨로 접는다. exit 4(경고 동반 성공)·exit 5(락 충돌, 재시도
/// 가능)는 실패가 아니다 — [`crate::web::job::JobOutcome`]의 판정을 그대로 따른다.
pub fn level_for_outcome(outcome: JobOutcome) -> Level {
    match outcome {
        JobOutcome::Succeeded => Level::Ok,
        JobOutcome::SucceededWithWarnings => Level::Warn,
        JobOutcome::LockConflict => Level::Warn,
        JobOutcome::Rejected
        | JobOutcome::PrecheckFailed
        | JobOutcome::Failed
        | JobOutcome::UnexpectedExit(_)
        | JobOutcome::Signaled(_)
        | JobOutcome::Unknown => Level::Fail,
    }
}

/// 잡이 실패/거부/락충돌로 끝났을 때의 안내 배너.
pub fn outcome_notice(lang: Lang, outcome: JobOutcome, stderr_excerpt: &str) -> Markup {
    let level = level_for_outcome(outcome);
    let headline = match outcome {
        JobOutcome::LockConflict => lang.sel(
            "Another x-backup process is using this profile right now.",
            "지금 다른 x-backup 프로세스가 이 프로파일을 쓰고 있습니다.",
        ),
        JobOutcome::Rejected => lang.sel(
            "peek rejected this request (bad arguments).",
            "peek가 이 요청을 거부했습니다(잘못된 인자).",
        ),
        JobOutcome::PrecheckFailed => lang.sel(
            "peek could not reach the source.",
            "peek가 소스에 연결하지 못했습니다.",
        ),
        JobOutcome::Failed => lang.sel("peek failed.", "peek가 실패했습니다."),
        _ => lang.sel(
            "peek ended unexpectedly.",
            "peek가 예상치 못하게 종료됐습니다.",
        ),
    };
    components::notice(
        level,
        headline,
        html! {
            p class="mono" { "exit: " (outcome.label()) }
            @if !stderr_excerpt.is_empty() {
                pre class="logdump" { (stderr_excerpt) }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;

    fn en() -> Lang {
        Lang::En
    }

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    // ---- 마스킹 렌더 ----

    #[test]
    fn shape_markup_never_leaks_string_content() {
        let doc = serde_json::json!({ "email": "person@example.com", "age": 30 });
        let out = shape_markup(en(), &doc, 0).into_string();
        assert!(!out.contains("person@example.com"), "원문이 샜다: {out}");
        // "person@example.com" = 18자. 길이는 원문이 아니라 크기만 드러내는 표기다.
        assert!(out.contains("string(18)"), "길이 표기 누락: {out}");
        assert!(out.contains("email"), "키는 보여야 한다: {out}");
        assert!(out.contains("number"), "숫자 타입 표기 누락: {out}");
    }

    #[test]
    fn shape_markup_examples_match_spec_tokens() {
        assert!(shape_markup(en(), &serde_json::json!(null), 0)
            .into_string()
            .contains("null"));
        assert!(shape_markup(en(), &serde_json::json!([1, 2, 3]), 0)
            .into_string()
            .contains("array[3]"));
        assert!(shape_markup(en(), &serde_json::json!("hi"), 0)
            .into_string()
            .contains("string(2)"));
    }

    #[test]
    fn shape_markup_recognizes_objectid_wrapper() {
        let oid = serde_json::json!({ "$oid": "507f1f77bcf86cd799439011" });
        let out = shape_markup(en(), &oid, 0).into_string();
        assert!(out.contains("objectid"), "objectid 타입 인식 실패: {out}");
        assert!(!out.contains("507f1f77bcf86cd799439011"), "원문이 샜다");
    }

    #[test]
    fn shape_markup_unknown_bson_tag_is_not_disguised_as_plain_object() {
        let weird = serde_json::json!({ "$futureType": "???" });
        let out = shape_markup(en(), &weird, 0).into_string();
        assert!(out.contains("bson-tagged"), "모르는 태그를 숨겼다: {out}");
    }

    // ---- 깊이 상한 ----

    /// 깊이 100+ 중첩 객체도 패닉 없이 렌더되고, 상한 지점에서 절단 표시가 남는다.
    #[test]
    fn deeply_nested_object_is_rendered_without_panic_and_marks_truncation() {
        let mut value = serde_json::json!("leaf");
        for _ in 0..150 {
            value = serde_json::json!({ "child": value });
        }
        let out = shape_markup(en(), &value, 0).into_string();
        assert!(out.contains("peek-trunc"), "깊이 상한 표시가 없다");

        let out2 = revealed_markup(en(), &value, 0, &empty_registry()).into_string();
        assert!(
            out2.contains("peek-trunc"),
            "원문 렌더도 깊이 상한을 표시해야 함"
        );
    }

    /// 깊이 100+ 중첩이 상한 내에서 렌더되는 시간이 합리적인 선을 넘지 않는다(알고리즘
    /// 폭발 감지).
    #[test]
    fn deeply_nested_rendering_completes_quickly() {
        let mut value = serde_json::json!("leaf");
        for _ in 0..500 {
            value = serde_json::json!({ "child": value, "sibling": 1 });
        }
        let started = std::time::Instant::now();
        let _ = shape_markup(en(), &value, 0).into_string();
        let _ = revealed_markup(en(), &value, 0, &empty_registry()).into_string();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "렌더가 비정상적으로 오래 걸렸다: {:?}",
            started.elapsed()
        );
    }

    // ---- 거대 필드 ----

    #[test]
    fn huge_string_field_is_truncated_when_revealed_and_marked() {
        let huge = "x".repeat(1_000_000);
        let doc = serde_json::json!({ "blob": huge });
        let out = revealed_markup(en(), &doc, 0, &empty_registry()).into_string();
        assert!(out.contains("peek-trunc"), "절단 표시가 없다");
        assert!(
            out.len() < 1_000_000,
            "출력이 원문 크기만큼 커졌다 — 절단되지 않음"
        );
    }

    #[test]
    fn huge_string_field_shape_only_reports_length_without_content() {
        let huge = "y".repeat(1_000_000);
        let doc = serde_json::json!({ "blob": huge });
        let out = shape_markup(en(), &doc, 0).into_string();
        assert!(out.contains("string(1000000)"));
        assert!(
            !out.contains(&"y".repeat(100)),
            "마스킹 모드에 원문 조각이 샜다"
        );
    }

    // ---- 제어문자 · HTML ----

    #[test]
    fn control_characters_are_stripped_when_revealed() {
        let doc = serde_json::json!({ "note": "a\u{0007}b\nc\td" });
        let out = revealed_markup(en(), &doc, 0, &empty_registry()).into_string();
        assert!(!out.contains('\u{0007}'), "벨 문자가 남았다");
        assert!(out.contains("control characters removed"));
    }

    #[test]
    fn html_tags_are_escaped_not_stripped() {
        let doc = serde_json::json!({ "bio": "<img src=x onerror=\"alert(1)\">" });
        let out = revealed_markup(en(), &doc, 0, &empty_registry()).into_string();
        assert!(!out.contains("<img"), "이스케이프되지 않았다: {out}");
        assert!(out.contains("&lt;img"), "이스케이프 형태가 아니다: {out}");
    }

    #[test]
    fn hostile_key_is_escaped_too() {
        let doc = serde_json::json!({ "<script>alert(1)</script>": "v" });
        let out = shape_markup(en(), &doc, 0).into_string();
        assert!(
            !out.contains("<script>"),
            "키가 이스케이프되지 않았다: {out}"
        );
    }

    // ---- 비-UTF8(치환 문자) ----

    #[test]
    fn replacement_character_from_lossy_utf8_renders_without_panic() {
        let doc = serde_json::json!({ "field": "broken:\u{FFFD}\u{FFFD}end" });
        let out = revealed_markup(en(), &doc, 0, &empty_registry()).into_string();
        assert!(out.contains('\u{FFFD}'));
    }

    // ---- 필드 수천 개 ----

    #[test]
    fn thousands_of_fields_are_capped_and_marked() {
        let mut map = serde_json::Map::new();
        for i in 0..5000 {
            map.insert(format!("f{i}"), serde_json::json!(i));
        }
        let doc = Value::Object(map);
        let out = shape_markup(en(), &doc, 0).into_string();
        assert!(out.contains("peek-trunc"));
        assert!(out.matches("<dt").count() <= MAX_FIELDS_SHOWN);
    }

    #[test]
    fn many_array_items_are_capped_and_marked_when_revealed() {
        let items: Vec<Value> = (0..500).map(|i| serde_json::json!(i)).collect();
        let doc = serde_json::json!({ "list": items });
        let out = revealed_markup(en(), &doc, 0, &empty_registry()).into_string();
        assert!(out.contains("peek-trunc"));
    }

    // ---- 시크릿 레지스트리(원문 모드) ----

    #[test]
    fn revealed_markup_masks_registered_secret_in_value_and_key() {
        let mut registry = SecretRegistry::new();
        registry.register("supersecretpassword123");
        let doc = serde_json::json!({
            "supersecretpassword123": "supersecretpassword123",
        });
        let out = revealed_markup(en(), &doc, 0, &registry).into_string();
        assert!(
            !out.contains("supersecretpassword123"),
            "등록된 시크릿이 키/값 어느 쪽에서도 남으면 안 된다: {out}"
        );
        assert!(out.contains(crate::web::mask::REDACTED_PLACEHOLDER));
    }

    #[test]
    fn shape_markup_keeps_ordinary_field_keys_visible() {
        let doc = serde_json::json!({ "email": "a@b.com", "ssn": "000-00-0000" });
        let out = shape_markup(en(), &doc, 0).into_string();
        // 값은 감추지만 키(필드 이름)는 이 화면의 존재 이유이므로 항상 보인다.
        assert!(out.contains("email"));
        assert!(out.contains("ssn"));
        assert!(!out.contains("a@b.com"));
        assert!(!out.contains("000-00-0000"));
    }

    // ---- 마크업 규약 ----

    #[test]
    fn no_inline_style_or_color_literals() {
        let doc = serde_json::json!({ "a": 1, "b": [1, 2], "c": "x" });
        let rendered = [
            shape_markup(en(), &doc, 0).into_string(),
            revealed_markup(en(), &doc, 0, &empty_registry()).into_string(),
            picker_body(en(), &["prod".to_string()], None).into_string(),
            outcome_notice(en(), JobOutcome::Failed, "boom").into_string(),
        ];
        for out in rendered {
            assert!(!out.contains("style="), "인라인 스타일: {out}");
        }
    }
}
