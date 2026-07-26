//! `prune` 화면 마크업 — 폼 · dry-run 계획 · 결과.
//!
//! ## 이 화면이 다른 화면과 다른 점 — 계획을 먼저 보여준다
//! `verify`나 `doctor`는 "실행 → 결과"로 끝난다. `prune`은 되돌릴 수 없는 삭제이므로
//! **실행 전에 무엇이 지워질지**를 먼저 보여주고, 그 화면에서 확인을 받아야 실행한다.
//! 그래서 화면이 셋이다: 폼 → 계획(+확인) → 결과.
//!
//! 확인 부분의 마크업은 이 파일이 만들지 않는다 — [`crate::web::guard::DestructiveGuard`]가
//! 만든다. 이 파일은 "무엇이 지워지는가"(계획 표)를 그리고, 그 아래에 가드가 만든 확인
//! 블록을 그대로 이어 붙인다. 확인 문구·이름 재입력·토큰 필드가 세 화면(prune·migrate·
//! restore)에서 갈리지 않게 하려는 것이 그 모듈의 존재 이유다.
//!
//! ## 잔재(force_only)를 따로 표시하는 이유
//! `prune --json`의 각 대상에는 `force_only`가 실려 온다(`cli/handlers/prune.rs`의
//! `build_json`). 체인 삭제는 보존 정책이 만든 정상 결과이지만, 잔재(고아·incomplete)는
//! **정책이 아니라 사고의 흔적**이고 CLI에서도 `--force`가 있어야 지워진다. 같은 표에
//! 섞으면 운영자가 "정책대로 도는 중"과 "뭔가 잘못된 것이 쌓여 있다"를 구분하지 못하므로
//! 배지로 갈라 둔다.
//!
//! ## 모양은 CSS가 정한다
//! 이 파일은 색·간격을 지정하지 않는다(`view` 모듈 헤더 규약) — 레벨(의미)만
//! [`components`]에 넘기고 나머지는 `app.css`가 정한다.

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::routes::prune as route;
use crate::web::view::components::{self, Level};

/// 계획 표에 그릴 대상 하나 — 라우트가 자식 JSON에서 접어 넘긴다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTarget {
    /// `chain` · `orphan` · `incomplete` 등 CLI가 낸 분류 라벨.
    pub kind: String,
    /// 체인의 base 백업 id(잔재면 비어 있을 수 있다).
    pub base_id: String,
    /// 이 대상과 함께 지워지는 백업 id들.
    pub member_ids: Vec<String>,
    /// 사람이 읽을 삭제 사유(CLI가 만든 문장).
    pub reason: String,
    /// `--force`가 있어야 지워지는 잔재인가.
    pub force_only: bool,
}

/// 계획 화면에 필요한 값 묶음.
#[derive(Debug, Clone)]
pub struct Plan {
    /// 대상 프로파일.
    pub profile: String,
    /// 백업 저장 위치(마스킹된 표시용 문자열).
    pub store: String,
    /// 삭제 대상.
    pub targets: Vec<PlanTarget>,
    /// 보존되는 base id들.
    pub retained_base_ids: Vec<String>,
    /// 이 계획의 지문 — 실행 요청에 그대로 실려 돌아온다(TOCTOU 방지).
    pub fingerprint: String,
}

impl Plan {
    /// 실제로 지워질 백업 수(대상들의 멤버 합).
    pub fn total_backups(&self) -> usize {
        self.targets.iter().map(|t| t.member_ids.len()).sum()
    }

    /// 잔재(`--force` 필요) 대상이 하나라도 있는가.
    pub fn has_force_only(&self) -> bool {
        self.targets.iter().any(|t| t.force_only)
    }
}

/// `GET /prune` — 프로파일과 보존 기준을 고르는 폼.
pub fn form_body(
    lang: Lang,
    profiles: &[String],
    selected: Option<&str>,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::PRUNE_TITLE, Some(lang.sel(
            "Preview what a retention policy would delete, then confirm to apply it.",
            "보존 정책이 무엇을 지울지 먼저 확인하고, 승인하면 적용합니다.",
        ))))
        @if let Some(notice) = notice { (notice) }
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("Plan a prune", "prune 계획")) } },
            html! {
                form method="post" action=(route::PRUNE_PLAN_PATH) {
                    div class="field" {
                        label class="field__label mono" for="f-profile" { "profile" }
                        select id="f-profile" name="profile" required {
                            @if profiles.is_empty() {
                                option value="" { (lang.sel("(no profiles in config)", "(config에 프로파일이 없습니다)")) }
                            }
                            @for name in profiles {
                                @if Some(name.as_str()) == selected {
                                    option value=(name) selected { (name) }
                                } @else {
                                    option value=(name) { (name) }
                                }
                            }
                        }
                    }
                    p class="field__hint" { (lang.sel(
                        "Leave the retention fields empty to use the profile's configured policy. Values here override it for this run only.",
                        "보존 항목을 비워 두면 프로파일에 설정된 정책을 씁니다. 여기 값은 이번 실행에만 적용되는 덮어쓰기입니다.",
                    )) }
                    @for (field, label, hint) in retention_fields(lang) {
                        div class="field" {
                            label class="field__label mono" for=(format!("f-{field}")) { (label) }
                            input id=(format!("f-{field}")) type="number" min="0" name=(field) autocomplete="off";
                            p class="field__hint" { (hint) }
                        }
                    }
                    button type="submit" { (lang.sel("Preview deletions", "삭제 대상 미리보기")) }
                }
            },
        ))
    }
}

/// 보존 기준 입력 필드 정의 — 폼과 라우트 파서가 **같은 이름 목록**을 봐야 한다.
fn retention_fields(lang: Lang) -> [(&'static str, &'static str, &'static str); 5] {
    [
        (
            route::FIELD_KEEP_FULL,
            "keep-full",
            lang.sel(
                "Keep the newest N full backups.",
                "최근 풀백업 N개를 보존합니다.",
            ),
        ),
        (
            route::FIELD_KEEP_DAYS,
            "keep-days",
            lang.sel(
                "Keep backups from the last D days.",
                "최근 D일치 백업을 보존합니다.",
            ),
        ),
        (
            route::FIELD_KEEP_LAST,
            "keep-last",
            lang.sel(
                "Keep the newest N backup sets (counted per chain).",
                "최신 백업 N벌을 보존합니다(체인 단위 누적).",
            ),
        ),
        (
            route::FIELD_RECOVERY_WINDOW_DAYS,
            "recovery-window-days",
            lang.sel(
                "Guarantee restore to any point in the last N days — keeps the boundary base too.",
                "지난 N일 임의 시점 복구를 보장합니다 — 경계가 되는 base까지 보존합니다.",
            ),
        ),
        (
            route::FIELD_MIN_REDUNDANCY,
            "min-redundancy",
            lang.sel(
                "Never go below M full chains, whatever the other rules say.",
                "다른 규칙과 무관하게 최소 M개 풀 체인은 남깁니다.",
            ),
        ),
    ]
}

/// dry-run 계획 화면 — 삭제 대상 표 + (가드가 만든) 확인 블록.
///
/// `confirm`은 [`crate::web::guard::DestructiveGuard::render_confirm`]이 만든 마크업이다.
/// 지울 것이 하나도 없으면 확인 블록을 붙이지 않는다 — 누를 수 있는 파괴적 버튼을 만들지
/// 않는 것이 "확인을 눌렀는데 아무 일도 안 일어났다"보다 정직하다.
pub fn plan_body(lang: Lang, plan: &Plan, confirm: Option<Markup>) -> Markup {
    html! {
        (components::page_head(route::PRUNE_TITLE, Some(lang.sel(
            "Nothing has been deleted yet — this is the plan.",
            "아직 아무것도 지우지 않았습니다 — 이것은 계획입니다.",
        ))))
        (components::meta_list(&[
            ("profile", plan.profile.clone()),
            ("store", plan.store.clone()),
            ("chains to delete", plan.targets.len().to_string()),
            ("backups to delete", plan.total_backups().to_string()),
            ("chains retained", plan.retained_base_ids.len().to_string()),
        ]))
        @if plan.targets.is_empty() {
            (components::notice(
                Level::Ok,
                lang.sel("Nothing to delete", "지울 것이 없습니다"),
                html! { p { (lang.sel(
                    "The retention policy keeps every backup in this store. Nothing would be removed.",
                    "보존 정책이 이 저장소의 모든 백업을 남깁니다. 지워질 것이 없습니다.",
                )) } },
            ))
        } @else {
            @if plan.has_force_only() {
                (components::notice(
                    Level::Warn,
                    lang.sel("Leftovers included", "잔재가 포함되어 있습니다"),
                    html! { p { (lang.sel(
                        "Some targets are orphaned or incomplete backups, not ordinary retention results. They are the trace of an interrupted run — worth a look before you approve.",
                        "일부 대상은 보존 정책의 정상 결과가 아니라 고아·incomplete 백업입니다. 중단된 실행이 남긴 흔적이므로 승인 전에 한 번 확인하세요.",
                    )) } },
                ))
            }
            (components::panel(
                html! { h3 class="panel__title" { (lang.sel("Targets", "삭제 대상")) } },
                html! {
                    table class="grid" {
                        thead {
                            tr {
                                th { "kind" }
                                th { "base" }
                                th { (lang.sel("members", "구성원")) }
                                th { (lang.sel("reason", "사유")) }
                            }
                        }
                        tbody {
                            @for target in &plan.targets {
                                tr {
                                    td {
                                        span class="mono" { (target.kind) }
                                        @if target.force_only {
                                            " " (components::badge(Level::Warn))
                                        }
                                    }
                                    td class="mono" { (display_or_dash(&target.base_id)) }
                                    td {
                                        span { (target.member_ids.len()) }
                                        @if !target.member_ids.is_empty() {
                                            details {
                                                summary { (lang.sel("show ids", "id 보기")) }
                                                ul class="plain" {
                                                    @for id in &target.member_ids {
                                                        li class="mono" { (id) }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    td { (target.reason) }
                                }
                            }
                        }
                    }
                },
            ))
            @if let Some(confirm) = confirm { (confirm) }
        }
        p { a href=(route::PRUNE_PATH) { (lang.sel("Start over", "처음부터 다시")) } }
    }
}

/// 계획이 바뀌어 실행을 거부했을 때의 안내.
///
/// 이 화면이 존재하는 이유가 곧 지문(fingerprint)의 존재 이유다 — 운영자가 본 계획과
/// 지금 실행될 계획이 다르면, **본 적 없는 삭제를 승인시키지 않는다.**
pub fn plan_changed_notice(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel(
            "The plan changed — nothing was deleted",
            "계획이 바뀌었습니다 — 아무것도 지우지 않았습니다",
        ),
        html! {
            p { (lang.sel(
                "Between the preview and your approval, this store changed — a backup finished, or another prune ran. The plan you approved is no longer the plan that would run, so nothing was deleted.",
                "미리보기와 승인 사이에 이 저장소가 바뀌었습니다 — 백업이 끝났거나 다른 prune이 돌았습니다. 승인한 계획과 지금 실행될 계획이 다르므로 아무것도 지우지 않았습니다.",
            )) }
            p { (lang.sel(
                "The current plan is shown below. Review it and approve again if it is still what you want.",
                "아래가 지금의 계획입니다. 확인한 뒤 여전히 원하는 것이면 다시 승인하세요.",
            )) }
        },
    )
}

/// 폼 검증 실패 안내.
pub fn validation_notice(lang: Lang, message: &str) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel("Cannot plan this", "계획을 세울 수 없습니다"),
        html! { p { (message) } },
    )
}

/// 가드가 제출을 거부했을 때의 안내(문구는 가드가 만든다).
pub fn guard_rejection_notice(lang: Lang, message: &str) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel("Confirmation refused", "확인이 거부됐습니다"),
        html! { p { (message) } },
    )
}

/// 잘못된 잡 id.
pub fn malformed_id(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel("Malformed job id", "잘못된 잡 id"),
        html! { p { (lang.sel(
            "That is not a job id this console issued.",
            "이 콘솔이 발급한 잡 id 형식이 아닙니다.",
        )) } },
    )
}

/// 모르는 잡.
pub fn unknown_job(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel("Unknown job", "모르는 잡"),
        html! { p { (lang.sel(
            "No prune job with that id is in this console's history.",
            "그 id의 prune 잡이 이 콘솔 이력에 없습니다.",
        )) } },
    )
}

/// 빈 문자열은 표에서 `-`로 그린다.
fn display_or_dash(value: &str) -> &str {
    if value.is_empty() {
        "-"
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        Plan {
            profile: "prod".to_string(),
            store: "local:/srv/backups".to_string(),
            targets: vec![
                PlanTarget {
                    kind: "chain".to_string(),
                    base_id: "base-1".to_string(),
                    member_ids: vec!["base-1".to_string(), "incr-1".to_string()],
                    reason: "keep-full 2를 넘긴 오래된 체인".to_string(),
                    force_only: false,
                },
                PlanTarget {
                    kind: "orphan".to_string(),
                    base_id: String::new(),
                    member_ids: vec!["orphan-9".to_string()],
                    reason: "base 없는 증분".to_string(),
                    force_only: true,
                },
            ],
            retained_base_ids: vec!["base-2".to_string()],
            fingerprint: "abc123".to_string(),
        }
    }

    #[test]
    fn plan_counts_members_not_targets() {
        let plan = sample_plan();
        assert_eq!(plan.targets.len(), 2);
        assert_eq!(plan.total_backups(), 3, "구성원 수를 세야 한다");
        assert!(plan.has_force_only());
    }

    #[test]
    fn plan_markup_lists_every_target_and_member() {
        let plan = sample_plan();
        let out = plan_body(Lang::En, &plan, None).into_string();
        for needle in [
            "chain",
            "orphan",
            "base-1",
            "incr-1",
            "orphan-9",
            "base 없는 증분",
        ] {
            assert!(out.contains(needle), "{needle}이 계획 표에 없다: {out}");
        }
    }

    /// 잔재가 섞여 있으면 그 사실을 배너로 알린다 — 표만 보고 지나치지 않게.
    #[test]
    fn leftovers_get_their_own_notice() {
        let plan = sample_plan();
        let with = plan_body(Lang::Ko, &plan, None).into_string();
        assert!(with.contains("잔재"), "잔재 배너가 없다");

        let mut clean = sample_plan();
        clean.targets.retain(|t| !t.force_only);
        let without = plan_body(Lang::Ko, &clean, None).into_string();
        assert!(
            !without.contains("잔재가 포함"),
            "잔재가 없는데 배너가 떴다"
        );
    }

    /// **지울 것이 없으면 확인 블록을 그리지 않는다** — 누를 수 있는 파괴적 버튼을
    /// 만들지 않는 것이 "눌렀는데 아무 일도 없다"보다 정직하다.
    #[test]
    fn an_empty_plan_never_renders_the_confirm_block() {
        let mut plan = sample_plan();
        plan.targets.clear();
        let confirm = html! { form id="the-confirm-form" {} };
        let out = plan_body(Lang::En, &plan, Some(confirm)).into_string();

        assert!(
            !out.contains("the-confirm-form"),
            "빈 계획에 확인 폼이 붙었다"
        );
        assert!(out.contains("Nothing to delete"));
    }

    #[test]
    fn a_non_empty_plan_carries_the_confirm_block() {
        let plan = sample_plan();
        let confirm = html! { form id="the-confirm-form" {} };
        let out = plan_body(Lang::En, &plan, Some(confirm)).into_string();
        assert!(out.contains("the-confirm-form"), "확인 폼이 붙지 않았다");
    }

    /// 마크업에 색·인라인 스타일이 없다(모양은 CSS 몫).
    #[test]
    fn markup_carries_no_presentation() {
        let out = plan_body(Lang::Ko, &sample_plan(), None).into_string();
        assert!(!out.contains("style="), "인라인 스타일이 있다: {out}");
        assert!(!out.contains('#'), "색 리터럴로 보이는 값이 있다: {out}");
    }

    /// 적대적인 문자열이 그대로 실행되지 않는다(maud 자동 이스케이프).
    #[test]
    fn hostile_reason_is_escaped() {
        let mut plan = sample_plan();
        plan.targets[0].reason = "<script>alert(1)</script>".to_string();
        let out = plan_body(Lang::En, &plan, None).into_string();
        assert!(!out.contains("<script>alert"), "이스케이프되지 않았다");
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn form_lists_profiles_and_retention_fields() {
        let profiles = vec!["prod".to_string(), "stage".to_string()];
        let out = form_body(Lang::En, &profiles, Some("stage"), None).into_string();
        assert!(
            out.contains(r#"value="stage" selected"#),
            "선택 상태가 반영되지 않았다"
        );
        for field in [
            route::FIELD_KEEP_FULL,
            route::FIELD_KEEP_DAYS,
            route::FIELD_KEEP_LAST,
            route::FIELD_RECOVERY_WINDOW_DAYS,
            route::FIELD_MIN_REDUNDANCY,
        ] {
            assert!(
                out.contains(&format!(r#"name="{field}""#)),
                "{field} 필드 누락"
            );
        }
    }
}
