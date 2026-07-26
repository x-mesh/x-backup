//! `GET /catalog` 마크업 — 필터 폼 + 판정 배너 + 카탈로그 표 + 페이지네이션.
//!
//! ## 이 파일은 판정도 파싱도 하지 않는다
//! [`crate::web::routes::catalog`]가 자식을 spawn하고, JSON을 파싱하고, 필터·페이지네이션을
//! 끝낸 결과([`Outcome`](routes::Outcome))를 값으로 넘겨준다. 이 파일이 하는 일은 그 값을
//! 마크업으로 옮기는 것뿐이다([`crate::web::routes`] 모듈 헤더의 "3. 순수 렌더 함수" 규약).
//!
//! ## `chain_status`는 네 상태가 아니라 텍스트로 구분한다
//! [`components::Level`]은 화면 전체가 공유하는 유일한 상태 축이라 네 칸
//! (`ok`/`warn`/`fail`/`error`)뿐이다. 그런데 문제 있는 카탈로그 행은
//! `broken`/`incomplete`/`orphan`/`corrupt` 네 가지이므로 색만으로는 서로 구분할 수 없다.
//! 그래서 [`chain_level`]로 행 전체(`data-level`)는 심각도에 따라 두 단계(Fail=broken·corrupt,
//! Warn=incomplete·orphan)로만 묶고, **Chain 열에는 원문 상태를 대문자로 그대로 찍는다**
//! (`crate::cli::handlers::list`의 `chain_label`과 같은 표기 — CLI와 웹이 같은 어휘를
//! 쓴다). 색은 스캔용, 글자는 구분용이다(WCAG 1.4.1 — `components` 헤더와 같은 원칙).
//!
//! ## 모양은 담지 않는다
//! [`components`] 헤더의 계약을 그대로 따른다 — 상태는 `data-level` 토큰으로만 말하고,
//! 색·인라인 스타일 리터럴은 마크업에 없다. 새 CSS 클래스를 만들지 않고 이미 있는 것만
//! 쓴다(`.dtable`·`.dtable-scroll`·`.field`·`.actions`·`.tag`·`.muted`·`.mono`·`.num`).
//!
//! ## 해석 불가 행은 표에서 사라지지 않는다
//! [`CatalogEntry::Unparseable`]은 정상 행과 같은 표 안에, [`Level::Error`] 배지로
//! `colspan`을 걸어 눈에 띄게 남는다 — 목록 한가운데 빠진 행처럼 보이면 운영자가 그
//! 존재조차 모른다.

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::mask::SecretRegistry;
use crate::web::routes::catalog::{self as routes, CatalogEntry, CatalogParams, Outcome, PageInfo};
use crate::web::view::components::{self, Level};

/// 카탈로그 표 한 행에서 사람이 읽는 시각으로 축약한다(`crate::cli::handlers::list::short_created`와
/// 같은 변환 — 판정이 아니라 순수 문자열 자르기라 이 화면이 직접 반복해도 도메인 로직
/// 재구현이 아니다). `"2026-06-14T14:56:11.354264+00:00"` → `"2026-06-14 14:56:11"`.
fn short_created(raw: Option<&str>) -> String {
    match raw {
        None => "-".to_string(),
        Some(ts) => ts.replacen('T', " ", 1).chars().take(19).collect(),
    }
}

/// `chain_status` 문자열을 행 전체를 물들이는 [`Level`]로 접는다. 모르는 값은 `Level::Error`다
/// (`routes::doctor::level_from_status`와 같은 판단 — 모르는 상태를 초록으로 떨어뜨리지 않는다).
fn chain_level(status: &str) -> Level {
    match status {
        "ok" => Level::Ok,
        "broken" | "corrupt" => Level::Fail,
        "incomplete" | "orphan" => Level::Warn,
        _ => Level::Error,
    }
}

/// Chain 열에 그대로 찍는 대문자 라벨 — CLI의 `chain_label`과 같은 표기.
fn chain_label(status: &str) -> String {
    match status {
        "broken" => "BROKEN".to_string(),
        "incomplete" => "INCOMPLETE".to_string(),
        "orphan" => "ORPHAN".to_string(),
        "corrupt" => "CORRUPT".to_string(),
        "ok" => "OK".to_string(),
        // 모르는 값은 원문을 그대로 보여준다 — 표기를 지어내지 않는다.
        other => other.to_string(),
    }
}

/// 바이트를 사람이 읽는 크기로. `crate::engine::mongo::status::human_bytes` 재사용 —
/// 순수 산술 포맷터라 DB 연결/도구 실행이 전혀 없고, 리더 지시서가 명시적으로 재사용을
/// 지시했다(이 화면이 `crate::engine`을 "호출"한다고 볼 계산이 아니다).
fn human_size(bytes: u64) -> String {
    // stored_size_bytes는 u64이지만 human_bytes는 i64를 받는다 — manifest 크기가
    // i64::MAX(약 8 EiB)를 넘을 일은 없으므로 saturating 변환으로 충분하다.
    crate::engine::mongo::status::human_bytes(i64::try_from(bytes).unwrap_or(i64::MAX))
}

/// `GET /catalog` 본문.
pub fn render(
    lang: Lang,
    params: &CatalogParams,
    profiles: &[String],
    outcome: &Outcome,
    registry: &SecretRegistry,
) -> Markup {
    let subtitle = lang.sel(
        "Full backup and incremental chain catalog for one destination, read via `list --json`.",
        "하나의 destination에 대한 백업·증분 체인 카탈로그입니다(`list --json` 경유).",
    );
    html! {
        (components::page_head(routes::CATALOG_TITLE, Some(subtitle)))
        (filter_form(lang, Some(params), profiles))
        @match outcome {
            Outcome::Loaded { verdict, store, page_rows, page_info } => {
                (components::verdict_banner(verdict.level(), &verdict.headline(lang), verdict.detail(lang).as_deref()))
                (loaded_body(lang, params, &registry.mask(store), page_rows, *page_info, registry))
            }
            Outcome::Unreadable { verdict, error, stderr } => {
                (components::verdict_banner(Level::Error, &error.explain(lang), verdict.detail(lang).as_deref()))
                (unreadable_notice(lang, stderr, registry))
            }
            Outcome::Unavailable { error } => {
                (components::verdict_banner(Level::Error, &error.explain(lang), None))
            }
        }
    }
}

/// 쿼리 검증에 실패했을 때(400)의 본문 — 입력 문자열을 되돌려 그리지 않는다
/// (`routes::jobs::malformed_id`와 같은 습관: 남의 문자열을 반사하지 않는다).
pub fn invalid_query_body(lang: Lang, profiles: &[String], message: &str) -> Markup {
    html! {
        (components::page_head(routes::CATALOG_TITLE, None))
        (components::notice(Level::Error, lang.sel("Invalid filter", "잘못된 필터"), html! {
            p { (message) }
        }))
        (filter_form(lang, None, profiles))
    }
}

/// 필터·정렬 폼. `current`가 없으면(쿼리 검증 실패 직후) 전부 기본값으로 그린다.
fn filter_form(lang: Lang, current: Option<&CatalogParams>, profiles: &[String]) -> Markup {
    let selected_profile = current.and_then(|p| p.profile.as_ref()).map(|p| p.as_str());
    let sort_is_size = current
        .map(|p| p.sort == crate::cli::args::ListSort::Size)
        .unwrap_or(false);
    let asc = current.map(|p| p.asc).unwrap_or(false);
    let type_sel = current.and_then(|p| p.type_filter.as_deref());
    let engine_sel = current.and_then(|p| p.engine_filter.as_deref());

    html! {
        form method="get" action=(routes::CATALOG_PATH) class="cfg-form" {
            div class="field" {
                label class="field__label mono" for="f-profile" { "profile" }
                select id="f-profile" name="profile" {
                    option value="" selected[selected_profile.is_none()] {
                        (lang.sel("(default profile)", "(기본 프로파일)"))
                    }
                    @for profile in profiles {
                        option value=(profile) selected[selected_profile == Some(profile.as_str())] { (profile) }
                    }
                }
            }
            div class="field" {
                label class="field__label mono" for="f-sort" { "sort" }
                select id="f-sort" name="sort" {
                    option value="created" selected[!sort_is_size] { "created" }
                    option value="size" selected[sort_is_size] { "size" }
                }
            }
            div class="field" {
                label {
                    input type="checkbox" name="asc" value="1" checked[asc];
                    " asc"
                }
                p class="field__hint" { (lang.sel(
                    "Default is descending (newest/largest first).",
                    "기본은 내림차순입니다(최신/큰 것이 위).",
                )) }
            }
            div class="field" {
                label class="field__label mono" for="f-type" { "type" }
                select id="f-type" name="type" {
                    option value="" selected[type_sel.is_none()] { (lang.sel("(all)", "(전체)")) }
                    @for t in ["full", "incr", "orphan", "corrupt"] {
                        option value=(t) selected[type_sel == Some(t)] { (t) }
                    }
                }
            }
            div class="field" {
                label class="field__label mono" for="f-engine" { "engine" }
                select id="f-engine" name="engine" {
                    option value="" selected[engine_sel.is_none()] { (lang.sel("(all)", "(전체)")) }
                    @for e in ["postgresql", "mongodb", "mysql"] {
                        option value=(e) selected[engine_sel == Some(e)] { (e) }
                    }
                }
            }
            div class="actions" {
                button type="submit" { (lang.sel("Apply", "적용")) }
                a href=(routes::CATALOG_PATH) { (lang.sel("Reset", "초기화")) }
            }
        }
    }
}

/// 정상적으로 자식 출력을 읽은 경우의 본문 — store 위치·메타·표·페이지네이션.
fn loaded_body(
    lang: Lang,
    params: &CatalogParams,
    masked_store: &str,
    page_rows: &[CatalogEntry],
    page_info: PageInfo,
    registry: &SecretRegistry,
) -> Markup {
    html! {
        (components::meta_list(&[
            ("store", masked_store.to_string()),
            ("total", page_info.total.to_string()),
            ("matched", page_info.matched.to_string()),
            ("page", format!("{} / {}", page_info.page, page_info.total_pages)),
        ]))
        @if page_rows.is_empty() {
            div class="card" {
                p { (lang.sel(
                    "No entries match the current filter.",
                    "현재 필터에 맞는 항목이 없습니다.",
                )) }
            }
        } @else {
            (catalog_table(page_rows, registry))
        }
        (pagination(lang, params, page_info))
    }
}

/// 카탈로그 표. 행에 `data-level`을 실어 CSS가 행 전체를 물들인다(`routes::doctor`의
/// 항목 표와 같은 방식 — 배지 하나만으로는 행이 많아지면 눈이 상태를 잃는다).
fn catalog_table(page_rows: &[CatalogEntry], registry: &SecretRegistry) -> Markup {
    html! {
        div class="dtable-scroll" {
            table class="dtable" {
                thead {
                    tr {
                        th scope="col" { "Status" }
                        th scope="col" { "ID" }
                        th scope="col" { "Type" }
                        th scope="col" { "Engine" }
                        th scope="col" { "Created" }
                        th scope="col" { "Size" }
                        th scope="col" { "Chain" }
                        th scope="col" { "Base" }
                    }
                }
                tbody {
                    @for entry in page_rows {
                        @match entry {
                            CatalogEntry::Row(row) => {
                                @let level = chain_level(&row.chain_status);
                                tr data-level=(level.token()) {
                                    td { (components::badge(level)) }
                                    td class="mono key" { (registry.mask(&row.id)) }
                                    td {
                                        (registry.mask(&row.kind))
                                        @if row.promoted_from_gap {
                                            " " span class="tag" { "gap" }
                                        }
                                    }
                                    td { (registry.mask(&row.engine)) }
                                    td class="mono" { (short_created(row.created_at.as_deref())) }
                                    td class="num" { (human_size(row.stored_size_bytes)) }
                                    td class="mono" { (chain_label(&row.chain_status)) }
                                    td class="mono" { (row.base_id.as_deref().map(|b| registry.mask(b)).unwrap_or_else(|| "-".to_string())) }
                                }
                            }
                            CatalogEntry::Unparseable { id, reason } => {
                                tr data-level=(Level::Error.token()) {
                                    td { (components::badge(Level::Error)) }
                                    td class="mono key" {
                                        @match id {
                                            Some(id) => (registry.mask(id)),
                                            None => "?",
                                        }
                                    }
                                    td class="msg" colspan="6" {
                                        (lang_unreadable_prefix())
                                        (registry.mask(reason))
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// "해석 불가" 접두사 — 라벨성 문구라 함수로 분리해 표 렌더가 조금이라도 짧아 보이게 한다.
/// 설명 문장이 아니라 진단 태그이므로 언어 토글 없이 영문 고정(다른 기술 라벨과 같은 규약).
fn lang_unreadable_prefix() -> &'static str {
    "unparseable — "
}

/// 이전/다음 링크 + 페이지 표시. 필터·정렬은 [`routes::page_href`]가 그대로 실어 나른다.
fn pagination(lang: Lang, params: &CatalogParams, info: PageInfo) -> Markup {
    html! {
        p class="actions" {
            @if info.page > 1 {
                a href=(routes::page_href(params, info.page - 1)) { (lang.sel("Prev", "이전")) }
            } @else {
                span class="muted" { (lang.sel("Prev", "이전")) }
            }
            span class="muted" {
                (lang.sel("page", "페이지")) " " (info.page) " / " (info.total_pages)
            }
            @if info.page < info.total_pages {
                a href=(routes::page_href(params, info.page + 1)) { (lang.sel("Next", "다음")) }
            } @else {
                span class="muted" { (lang.sel("Next", "다음")) }
            }
        }
    }
}

/// stdout을 읽지 못했을 때의 진단 블록 — 종료 코드 판정(verdict)과 stderr 발췌를 함께 보여준다.
fn unreadable_notice(lang: Lang, stderr: &str, registry: &SecretRegistry) -> Markup {
    let masked = registry.mask(stderr);
    components::notice(
        Level::Error,
        lang.sel("Child diagnostics", "자식 프로세스 진단"),
        html! {
            @if masked.is_empty() {
                p class="muted" { (lang.sel("The child wrote nothing to stderr either.", "자식이 stderr에도 아무것도 쓰지 않았습니다.")) }
            } @else {
                pre class="logdump" { (masked) }
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::ListSort;
    use crate::web::job::ProfileName;
    use crate::web::routes::catalog::{CatalogRow, ReportError, Verdict};

    fn empty_registry() -> SecretRegistry {
        SecretRegistry::new()
    }

    fn params(
        profile: Option<&str>,
        sort: ListSort,
        asc: bool,
        type_filter: Option<&str>,
        engine_filter: Option<&str>,
        page: usize,
    ) -> CatalogParams {
        CatalogParams {
            profile: profile.map(|p| ProfileName::parse(p, Lang::En).unwrap()),
            sort,
            asc,
            type_filter: type_filter.map(str::to_string),
            engine_filter: engine_filter.map(str::to_string),
            page,
        }
    }

    fn default_params() -> CatalogParams {
        params(None, ListSort::Created, false, None, None, 1)
    }

    fn ok_row(id: &str) -> CatalogEntry {
        CatalogEntry::Row(CatalogRow {
            id: id.to_string(),
            kind: "full".to_string(),
            promoted_from_gap: false,
            engine: "mongodb".to_string(),
            created_at: Some("2026-06-14T14:56:11.354264+00:00".to_string()),
            stored_size_bytes: 2 * 1024 * 1024,
            chain_status: "ok".to_string(),
            base_id: None,
        })
    }

    fn page_info(page: usize, total_pages: usize, total: usize, matched: usize) -> PageInfo {
        PageInfo {
            page,
            total_pages,
            total,
            matched,
            page_size: routes::PAGE_SIZE,
        }
    }

    // ---- 시각·크기 표기 ----

    #[test]
    fn short_created_truncates_and_swaps_separator() {
        assert_eq!(
            short_created(Some("2026-06-14T14:56:11.354264+00:00")),
            "2026-06-14 14:56:11"
        );
        assert_eq!(short_created(None), "-");
    }

    #[test]
    fn human_size_formats_binary_units() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(2 * 1024 * 1024), "2.0 MiB");
    }

    // ---- chain_status 표현 ----

    /// 네 문제 상태가 서로 다른 라벨을 갖고(색만으로 뭉개지지 않는다), broken/corrupt는
    /// Fail, incomplete/orphan은 Warn으로 묶인다.
    #[test]
    fn chain_statuses_have_distinct_labels() {
        let labels: Vec<String> = ["ok", "broken", "incomplete", "orphan", "corrupt"]
            .iter()
            .map(|s| chain_label(s))
            .collect();
        let mut sorted = labels.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), labels.len(), "라벨이 서로 겹친다: {labels:?}");

        assert_eq!(chain_level("broken"), Level::Fail);
        assert_eq!(chain_level("corrupt"), Level::Fail);
        assert_eq!(chain_level("incomplete"), Level::Warn);
        assert_eq!(chain_level("orphan"), Level::Warn);
        assert_eq!(chain_level("ok"), Level::Ok);
    }

    /// 모르는 chain_status는 초록(Ok)으로 떨어지지 않는다.
    #[test]
    fn unknown_chain_status_is_never_ok() {
        for weird in ["", "OK", "unknown-future-state"] {
            assert_eq!(
                chain_level(weird),
                Level::Error,
                "'{weird}'가 조용히 통과했다"
            );
        }
    }

    // ---- 표 렌더 ----

    /// 네 문제 상태와 정상 상태가 실제 렌더에서 서로 다른 `data-level`/텍스트로 나온다.
    #[test]
    fn table_renders_distinct_levels_and_text_for_each_status() {
        let statuses = ["ok", "broken", "incomplete", "orphan", "corrupt"];
        let rows: Vec<CatalogEntry> = statuses
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut row = ok_row(&format!("id-{i}"));
                let CatalogEntry::Row(r) = &mut row else {
                    unreachable!()
                };
                r.chain_status = s.to_string();
                row
            })
            .collect();
        let out = catalog_table(&rows, &empty_registry()).into_string();
        for status in statuses {
            assert!(
                out.contains(&status.to_uppercase()) || (status == "ok" && out.contains("OK")),
                "'{status}' 라벨 누락: {out}"
            );
        }
        // fail·warn·error·ok 최소 두 레벨 이상이 동시에 나타난다(전부 하나로 뭉개지지 않았다).
        assert!(out.contains(r#"data-level="fail""#));
        assert!(out.contains(r#"data-level="warn""#));
        assert!(out.contains(r#"data-level="ok""#));
    }

    /// gap 승격 full은 `full` 옆에 `gap` 태그가 붙는다(CLI의 `full(gap)` 표시와 같은 정보).
    #[test]
    fn gap_promoted_full_shows_gap_tag() {
        let mut row = ok_row("g1");
        let CatalogEntry::Row(r) = &mut row else {
            unreachable!()
        };
        r.promoted_from_gap = true;
        let out = catalog_table(&[row], &empty_registry()).into_string();
        assert!(
            out.contains(r#"class="tag""#) && out.contains("gap"),
            "{out}"
        );
    }

    /// **해석 불가 행은 숨겨지지 않는다** — id가 있으면 보이고, Error 레벨로 표시된다.
    #[test]
    fn unparseable_row_is_shown_not_hidden() {
        let entries = vec![
            ok_row("good"),
            CatalogEntry::Unparseable {
                id: Some("bad-1".to_string()),
                reason: "missing or malformed field(s): engine".to_string(),
            },
        ];
        let out = catalog_table(&entries, &empty_registry()).into_string();
        assert!(out.contains("good"), "정상 행이 사라졌다");
        assert!(out.contains("bad-1"), "해석 불가 행의 id가 사라졌다: {out}");
        assert!(out.contains("unparseable"), "해석 불가 표식이 없다: {out}");
        assert!(
            out.contains("missing or malformed field(s): engine"),
            "이유가 사라졌다: {out}"
        );
        assert!(out.contains(r#"data-level="error""#));
    }

    /// id조차 없는 해석 불가 행도 표에서 사라지지 않는다(`?`로 표시).
    #[test]
    fn unparseable_row_without_id_still_renders() {
        let entries = vec![CatalogEntry::Unparseable {
            id: None,
            reason: "missing or malformed field(s): id".to_string(),
        }];
        let out = catalog_table(&entries, &empty_registry()).into_string();
        assert!(out.contains(">?<"), "id 없음 표식 누락: {out}");
    }

    /// 시크릿이 카탈로그 값(엔진·id 등)에 섞여 들어와도 마스킹된다(2차 방어).
    #[test]
    fn registered_secrets_are_masked_in_row_fields() {
        const FAKE: &str = "NOT-A-REAL-SECRET-catalogview-8d21f4";
        let mut registry = SecretRegistry::new();
        assert!(registry.register(FAKE));
        let mut row = ok_row(&format!("id-with-{FAKE}"));
        let CatalogEntry::Row(r) = &mut row else {
            unreachable!()
        };
        r.base_id = Some(FAKE.to_string());
        let out = catalog_table(&[row], &registry).into_string();
        assert!(!out.contains(FAKE), "시크릿 원문이 표에 남았다: {out}");
        assert!(out.matches(crate::web::mask::REDACTED_PLACEHOLDER).count() >= 2);
    }

    // ---- 전체 렌더 ----

    /// 정상 로드는 판정 배너·store·표·페이지네이션을 모두 낸다.
    #[test]
    fn render_loaded_shows_banner_meta_table_and_pagination() {
        let outcome = Outcome::Loaded {
            verdict: Verdict::Clean,
            store: "/srv/backups/prod".to_string(),
            page_rows: vec![ok_row("a")],
            page_info: page_info(1, 3, 120, 60),
        };
        let out = render(
            Lang::En,
            &default_params(),
            &["prod".to_string()],
            &outcome,
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains(r#"class="verdict" data-level="ok""#), "{out}");
        assert!(out.contains("/srv/backups/prod"));
        assert!(out.contains("120"), "total 누락");
        assert!(out.contains("60"), "matched 누락");
        assert!(out.contains("1 / 3"), "page 표시 누락: {out}");
        assert!(out.contains(r#"<option value="prod">prod</option>"#));
    }

    /// exit 4(Warned)는 실패색이 아니라 경고색이고, "성공"이라는 사실이 문장에 있다.
    #[test]
    fn warned_verdict_is_not_painted_as_failure() {
        let outcome = Outcome::Loaded {
            verdict: Verdict::Warned,
            store: "/srv/backups/prod".to_string(),
            page_rows: vec![],
            page_info: page_info(1, 1, 1, 0),
        };
        let out = render(
            Lang::En,
            &default_params(),
            &[],
            &outcome,
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains(r#"data-level="warn""#));
        assert!(!out.contains(r#"class="verdict" data-level="fail""#));
        assert!(
            out.contains("is complete") || out.contains("success"),
            "{out}"
        );
    }

    /// stdout을 못 읽은 경우도 200 화면으로 접히고(패닉 없음) 진단이 남는다.
    #[test]
    fn render_unreadable_shows_diagnostics() {
        let outcome = Outcome::Unreadable {
            verdict: Verdict::Misconfigured,
            error: ReportError::Empty,
            stderr: "ERROR: destination.type이 지정되지 않았습니다".to_string(),
        };
        let out = render(
            Lang::En,
            &default_params(),
            &[],
            &outcome,
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains(r#"data-level="error""#));
        assert!(out.contains("no output"), "{out}");
        assert!(out.contains("destination.type"), "{out}");
    }

    /// 빈 페이지(필터에 아무것도 안 맞음)는 표 대신 안내 문장을 낸다.
    #[test]
    fn empty_page_shows_placeholder_not_empty_table() {
        let outcome = Outcome::Loaded {
            verdict: Verdict::Clean,
            store: "/srv".to_string(),
            page_rows: vec![],
            page_info: page_info(1, 1, 10, 0),
        };
        let out = render(
            Lang::En,
            &default_params(),
            &[],
            &outcome,
            &empty_registry(),
        )
        .into_string();
        assert!(out.contains("No entries match"));
        assert!(!out.contains("<table"), "빈 표를 그렸다: {out}");
    }

    /// 필터 폼이 현재 선택값을 그대로 반영한다(선택 유지 — 폼을 냈을 때 조건을 잃지 않는다).
    #[test]
    fn filter_form_reflects_current_selection() {
        let p = params(
            Some("prod"),
            ListSort::Size,
            true,
            Some("orphan"),
            Some("mongodb"),
            2,
        );
        let out = filter_form(
            Lang::En,
            Some(&p),
            &["prod".to_string(), "staging".to_string()],
        )
        .into_string();
        assert!(
            out.contains(r#"<option value="prod" selected>prod</option>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<option value="size" selected>size</option>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<option value="orphan" selected>orphan</option>"#),
            "{out}"
        );
        assert!(
            out.contains(r#"<option value="mongodb" selected>mongodb</option>"#),
            "{out}"
        );
        assert!(out.contains("checked"), "asc 체크 상태 누락");
    }

    /// 페이지네이션 링크가 현재 필터·정렬을 그대로 실어 나른다(필터를 잃지 않는다).
    #[test]
    fn pagination_links_preserve_filters() {
        let p = params(None, ListSort::Size, true, Some("full"), None, 2);
        let out = pagination(Lang::En, &p, page_info(2, 5, 500, 250)).into_string();
        assert!(out.contains("sort=size"));
        assert!(out.contains("asc=1"));
        assert!(out.contains("type=full"));
        assert!(out.contains("page=1"), "이전 링크 누락: {out}");
        assert!(out.contains("page=3"), "다음 링크 누락: {out}");
    }

    /// 첫/마지막 페이지에서는 반대 방향 링크가 비활성 텍스트로 바뀐다(깨진 링크 없음).
    #[test]
    fn pagination_disables_edges() {
        let p = default_params();
        let first = pagination(Lang::En, &p, page_info(1, 3, 100, 100)).into_string();
        assert!(
            !first.contains("Prev</a>"),
            "첫 페이지에 활성 Prev가 있다: {first}"
        );
        let last = pagination(Lang::En, &p, page_info(3, 3, 100, 100)).into_string();
        assert!(
            !last.contains("Next</a>"),
            "마지막 페이지에 활성 Next가 있다: {last}"
        );
    }

    // ---- 안전성 ----

    /// 마크업에 색 리터럴·인라인 스타일이 없다([`components`] 계약).
    #[test]
    fn markup_carries_no_presentation() {
        let outcome = Outcome::Loaded {
            verdict: Verdict::Warned,
            store: "/srv".to_string(),
            page_rows: vec![ok_row("a")],
            page_info: page_info(1, 1, 1, 1),
        };
        let out = render(
            Lang::Ko,
            &default_params(),
            &["p".to_string()],
            &outcome,
            &empty_registry(),
        )
        .into_string();
        assert!(!out.contains("style="), "인라인 스타일이 들어갔다: {out}");
        assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
    }

    /// 자식이 만든 적대적 문자열(id·엔진·stderr)이 전부 이스케이프된다 — 저장형 XSS 없음.
    #[test]
    fn hostile_child_output_is_escaped() {
        let hostile = r#"<img src=x onerror="alert(1)">"#;
        let mut row = ok_row(hostile);
        let CatalogEntry::Row(r) = &mut row else {
            unreachable!()
        };
        r.engine = hostile.to_string();
        r.base_id = Some(hostile.to_string());
        let table = catalog_table(&[row], &empty_registry()).into_string();
        assert!(
            !table.contains("<img"),
            "표에서 이스케이프되지 않았다: {table}"
        );
        assert!(table.contains("&lt;img"));

        let unreadable = Outcome::Unreadable {
            verdict: Verdict::Failed,
            error: ReportError::Malformed(hostile.to_string()),
            stderr: hostile.to_string(),
        };
        let out = render(
            Lang::En,
            &default_params(),
            &[],
            &unreadable,
            &empty_registry(),
        )
        .into_string();
        assert!(
            !out.contains("<img"),
            "진단 블록에서 이스케이프되지 않았다: {out}"
        );
        assert!(out.contains("&lt;img"));
    }
}
