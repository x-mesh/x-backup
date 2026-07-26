//! `restore` 화면 마크업 — 폼 · dry-run 계획 · 확인.
//!
//! ## 이 화면이 반드시 말해야 하는 것 — **어디로 복구되는가**
//! `restore`의 기본 동작은 **백업을 떠온 그 프로파일의 source로 되돌리는 것**(in-place)이다.
//! 즉 아무것도 고르지 않으면 프로덕션 위에 쓴다. 그래서 계획 화면의 첫 줄이 대상이고,
//! 그 옆에 **그 대상이 어디서 왔는지**(origin)를 함께 적는다 — `--target-profile`로 고른
//! 것인지, 아무것도 고르지 않아 프로파일 자신으로 되돌아가는 것인지.
//!
//! 그 규약은 CLI가 이미 갖고 있다([`crate::cli::output::RestoreTargetOrigin`] —
//! stderr에 `→ 복구 대상 <가린 URI> (출처)` 한 줄을 찍는다). 이 화면은 같은 두 값을
//! 같은 뜻으로 보여준다. **URI는 자식이 이미 가려서 준다**(`destination` 필드가
//! `redact_uri`를 거친 값이다) — 이 파일이 다시 가리지 않는 이유는, 가리는 규칙이 두 곳에
//! 생기면 한쪽만 고쳐졌을 때 조용히 새기 때문이다.
//!
//! ## 두 가지 복구 모드
//! - **백업 선택** — 특정 백업 하나를 그대로 되돌린다.
//! - **시점(PITR)** — `--at` 시각까지 재생한다. 이때 화면은 **요청한 시각과 실제로 도달할
//!   시각을 함께** 보여준다(`decided_wall_clock`). 두 값이 다른 것이 정상이다(PITR은
//!   요청 시각 **이하**의 최대 oplog 지점으로 내림 매핑한다) — 그 사실을 말하지 않으면
//!   운영자는 몇 초 차이를 버그로 읽는다.

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::routes::restore as route;
use crate::web::view::components::{self, Level};

/// 복구 대상이 어디서 왔는지 — [`crate::cli::output::RestoreTargetOrigin`]의 웹 쪽 대응.
///
/// CLI의 그 타입을 그대로 쓰지 않는 이유: 그쪽은 `Flag`(원시 `--target` URI) 변종을 갖는데
/// 이 콘솔은 그 경로를 제공하지 않는다([`crate::web::routes::restore`] 헤더). 웹에 없는
/// 상태를 표현할 수 있는 타입을 쓰면 "그 경우엔 뭘 그리지?"가 영원히 열린 질문으로 남는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetOrigin {
    /// 아무것도 고르지 않았다 — 프로파일 자신의 source로 되돌린다.
    InPlace,
    /// `--target-profile <name>`.
    Profile(String),
}

impl TargetOrigin {
    /// 대상 옆 괄호에 들어가는 출처 라벨.
    pub fn label(&self, lang: Lang) -> String {
        match self {
            TargetOrigin::InPlace => lang.sel("profile source", "프로파일 source").to_string(),
            TargetOrigin::Profile(name) => format!("--target-profile: {name}"),
        }
    }

    /// in-place인가 — 화면이 경고 수위를 올리는 판정에 쓴다.
    pub fn is_in_place(&self) -> bool {
        matches!(self, TargetOrigin::InPlace)
    }
}

/// PITR 시점 정보.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitrPoint {
    /// 운영자가 요청한 시각(입력 그대로).
    pub requested: String,
    /// 실제로 도달하는 시각(자식이 내림 매핑한 결과).
    pub decided: String,
    /// 재생할 증분 슬라이스 수.
    pub replay_slices: usize,
}

/// dry-run 계획.
#[derive(Debug, Clone)]
pub struct Plan {
    /// 대상 프로파일(백업을 읽어올 곳).
    pub profile: String,
    /// **가려진** 복구 대상 URI — 자식이 이미 가려서 준 값이다.
    pub destination: String,
    /// 그 대상이 어디서 왔는지.
    pub origin: TargetOrigin,
    /// 복구에 쓰이는 base 백업 id.
    pub backup_id: String,
    /// PITR이면 시점 정보, 아니면 `None`.
    pub pitr: Option<PitrPoint>,
    /// 대상에 이미 있어 충돌하는 네임스페이스.
    pub conflicting: Vec<String>,
    /// 서버 버전 경고.
    pub version_warning: Option<String>,
    /// 선택적 네임스페이스 필터(`--only`).
    pub only: Option<String>,
    /// 이 계획의 지문.
    pub fingerprint: String,
}

impl Plan {
    /// 덮어쓸 것이 있는가.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicting.is_empty()
    }
}

/// `GET /restore` — 폼.
pub fn form_body(
    lang: Lang,
    profiles: &[String],
    prefill: &Prefill,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::RESTORE_TITLE, Some(lang.sel(
            "Restore a backup, or replay to a point in time. By default this writes back into the profile's own source — the production server the backup came from.",
            "백업을 복구하거나 특정 시점까지 재생합니다. 아무것도 고르지 않으면 백업을 떠온 그 프로파일의 source — 즉 원본 서버에 씁니다.",
        ))))
        @if let Some(notice) = notice { (notice) }
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("Plan a restore", "복구 계획")) } },
            html! {
                form method="post" action=(route::RESTORE_PLAN_PATH) {
                    (select_field(lang, route::FIELD_PROFILE, "profile", profiles, prefill.profile.as_deref(), lang.sel(
                        "The profile whose backups are read.",
                        "백업을 읽어올 프로파일입니다.",
                    ), false))

                    div class="field" {
                        label class="field__label mono" for="f-id" { "backup id" }
                        input id="f-id" type="text" name=(route::FIELD_ID) autocomplete="off"
                            value=(prefill.id.clone().unwrap_or_default());
                        p class="field__hint" { (lang.sel(
                            "Restore this backup exactly. Leave empty to use the newest full backup, or fill the point-in-time field below instead.",
                            "이 백업을 그대로 복구합니다. 비우면 최신 풀백업을 쓰며, 아래 시점 항목을 대신 채울 수도 있습니다.",
                        )) }
                    }

                    div class="field" {
                        label class="field__label mono" for="f-at" { "point in time" }
                        input id="f-at" type="text" name=(route::FIELD_AT) autocomplete="off"
                            placeholder="2026-06-12T13:00:00Z"
                            value=(prefill.at.clone().unwrap_or_default());
                        p class="field__hint" { (lang.sel(
                            "RFC 3339 with a timezone (the trailing Z means UTC). Replays increments up to this moment — the console lands on the newest recorded point at or before it.",
                            "타임존을 포함한 RFC 3339 형식입니다(끝의 Z는 UTC). 이 시각까지 증분을 재생하며, 실제로는 그 시각 이하의 가장 최근 기록 지점에 도달합니다.",
                        )) }
                    }

                    (select_field(lang, route::FIELD_TARGET_PROFILE, "restore into", profiles, prefill.target_profile.as_deref(), lang.sel(
                        "Leave unselected to restore in place — back into the profile's own source. Pick another profile to restore somewhere else.",
                        "고르지 않으면 제자리 복구입니다 — 프로파일 자신의 source로 되돌립니다. 다른 곳으로 복구하려면 프로파일을 고르세요.",
                    ), true))

                    div class="field" {
                        label class="field__label mono" for="f-only" { "only" }
                        input id="f-only" type="text" name=(route::FIELD_ONLY) autocomplete="off"
                            placeholder="db.collection"
                            value=(prefill.only.clone().unwrap_or_default());
                        p class="field__hint" { (lang.sel(
                            "Optional — restore just this namespace.",
                            "선택 — 이 네임스페이스만 복구합니다.",
                        )) }
                    }

                    div class="field" {
                        label class="field__label mono" for="f-from" { "from" }
                        input id="f-from" type="text" name=(route::FIELD_FROM) autocomplete="off";
                        p class="field__hint" { (lang.sel(
                            "Optional — which destination to read from when the profile has several. Defaults to the first.",
                            "선택 — destination이 여러 개인 프로파일에서 어디서 읽을지 고릅니다. 기본은 첫 번째입니다.",
                        )) }
                    }

                    button type="submit" { (lang.sel("Preview the restore", "복구 미리보기")) }
                }
            },
        ))
    }
}

/// 폼 프리필 값 — 카탈로그 링크(`?id=`)와 검증 실패 후 되채우기에 함께 쓴다.
#[derive(Debug, Default, Clone)]
pub struct Prefill {
    /// 프로파일.
    pub profile: Option<String>,
    /// 백업 id.
    pub id: Option<String>,
    /// `--at`.
    pub at: Option<String>,
    /// `--target-profile`.
    pub target_profile: Option<String>,
    /// `--only`.
    pub only: Option<String>,
}

/// 선택 필드 하나. `optional`이면 "(고르지 않음)" 항목이 맨 위에 붙는다.
fn select_field(
    lang: Lang,
    field: &str,
    label: &str,
    profiles: &[String],
    selected: Option<&str>,
    hint: &str,
    optional: bool,
) -> Markup {
    html! {
        div class="field" {
            label class="field__label mono" for=(format!("f-{field}")) { (label) }
            select id=(format!("f-{field}")) name=(field) required[!optional] {
                @if optional {
                    @if selected.is_none_or(str::is_empty) {
                        option value="" selected { (lang.sel("(restore in place)", "(제자리 복구)")) }
                    } @else {
                        option value="" { (lang.sel("(restore in place)", "(제자리 복구)")) }
                    }
                }
                @if profiles.is_empty() && !optional {
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
            p class="field__hint" { (hint) }
        }
    }
}

/// dry-run 계획 화면.
pub fn plan_body(lang: Lang, plan: &Plan, confirm: Option<Markup>) -> Markup {
    html! {
        (components::page_head(route::RESTORE_TITLE, Some(lang.sel(
            "Nothing has been restored yet — this is the plan.",
            "아직 아무것도 복구하지 않았습니다 — 이것은 계획입니다.",
        ))))

        // **대상이 첫 줄이다** — 이 화면에서 가장 먼저 읽혀야 하는 값이다(모듈 헤더).
        (components::meta_list(&[
            ("restore into", format!("{} ({})", plan.destination, plan.origin.label(lang))),
            ("profile", plan.profile.clone()),
            ("backup", plan.backup_id.clone()),
            ("only", plan.only.clone().unwrap_or_else(|| "(everything)".to_string())),
        ]))

        @if plan.origin.is_in_place() {
            (components::notice(
                Level::Warn,
                lang.sel("This restores in place", "제자리 복구입니다"),
                html! { p { (lang.sel(
                    "No separate target was chosen, so this writes back into the profile's own source — the server the backup was taken from. If you meant to restore somewhere else, go back and pick a target profile.",
                    "별도의 대상을 고르지 않았으므로 프로파일 자신의 source — 즉 이 백업을 떠온 그 서버에 씁니다. 다른 곳으로 복구하려는 것이었다면 돌아가서 대상 프로파일을 고르세요.",
                )) } },
            ))
        }

        @if let Some(pitr) = &plan.pitr {
            (components::panel(
                html! { h3 class="panel__title" { (lang.sel("Point in time", "복구 시점")) } },
                html! {
                    (components::meta_list(&[
                        ("requested", pitr.requested.clone()),
                        ("will land on", pitr.decided.clone()),
                        ("slices to replay", pitr.replay_slices.to_string()),
                    ]))
                    p class="field__hint" { (lang.sel(
                        "The two times differ when nothing was recorded at the exact moment you asked for — the restore lands on the newest recorded point at or before it. That is normal, not an error.",
                        "요청한 바로 그 순간에 기록된 것이 없으면 두 시각이 다릅니다 — 그 시각 이하의 가장 최근 기록 지점에 도달합니다. 오류가 아니라 정상 동작입니다.",
                    )) }
                },
            ))
        }

        @if let Some(warning) = &plan.version_warning {
            (components::notice(
                Level::Warn,
                lang.sel("Server versions may not be compatible", "서버 버전이 호환되지 않을 수 있습니다"),
                html! { p { (warning) } },
            ))
        }

        @if plan.has_conflicts() {
            (components::notice(
                Level::Fail,
                lang.sel("The target already has data here", "대상에 이미 데이터가 있습니다"),
                html! {
                    p { (lang.sel(
                        "These namespaces exist on the restore target and will be written over.",
                        "아래 네임스페이스가 복구 대상에 이미 있으며 덮어써집니다.",
                    )) }
                    ul class="plain" {
                        @for ns in &plan.conflicting {
                            li class="mono" { (ns) }
                        }
                    }
                },
            ))
        }

        @if let Some(confirm) = confirm { (confirm) }
        p { a href=(route::RESTORE_PATH) { (lang.sel("Start over", "처음부터 다시")) } }
    }
}

/// 계획이 바뀌어 실행을 거부했을 때의 안내.
pub fn plan_changed_notice(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel(
            "The plan changed — nothing was restored",
            "계획이 바뀌었습니다 — 아무것도 복구하지 않았습니다",
        ),
        html! {
            p { (lang.sel(
                "Between the preview and your approval, something changed — a new backup arrived, or the target's contents moved. The plan you approved is no longer the plan that would run, so nothing was restored.",
                "미리보기와 승인 사이에 무언가 바뀌었습니다 — 새 백업이 도착했거나 대상의 내용이 달라졌습니다. 승인한 계획과 지금 실행될 계획이 다르므로 아무것도 복구하지 않았습니다.",
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
        lang.sel("Cannot plan this restore", "이 복구를 계획할 수 없습니다"),
        html! { p { (message) } },
    )
}

/// 가드가 제출을 거부했을 때의 안내.
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
            "No restore job with that id is in this console's history.",
            "그 id의 restore 잡이 이 콘솔 이력에 없습니다.",
        )) } },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        Plan {
            profile: "prod".to_string(),
            destination: "mongodb://***:***@db.internal:27017".to_string(),
            origin: TargetOrigin::Profile("dr".to_string()),
            backup_id: "bk-1".to_string(),
            pitr: None,
            conflicting: vec!["shop.orders".to_string()],
            version_warning: None,
            only: None,
            fingerprint: "fp".to_string(),
        }
    }

    /// **done_criteria**: 가려진 대상 URI와 origin 라벨이 함께 표시된다.
    #[test]
    fn the_plan_shows_the_masked_target_with_its_origin() {
        let out = plan_body(Lang::En, &sample_plan(), None).into_string();
        assert!(out.contains("db.internal:27017"), "대상 host가 없다");
        assert!(out.contains("***"), "가려진 형태가 아니다");
        assert!(
            out.contains("--target-profile: dr"),
            "origin 라벨이 없다: {out}"
        );
    }

    /// in-place 복구는 라벨이 다르고, 경고 배너가 붙는다 — 프로덕션 위에 쓰는 경우다.
    #[test]
    fn in_place_restores_say_so_loudly() {
        let mut plan = sample_plan();
        plan.origin = TargetOrigin::InPlace;
        let out = plan_body(Lang::Ko, &plan, None).into_string();
        assert!(out.contains("프로파일 source"), "in-place 라벨이 없다");
        assert!(out.contains("제자리 복구입니다"), "in-place 경고가 없다");

        // 대상이 따로 있으면 그 경고는 뜨지 않는다.
        let elsewhere = plan_body(Lang::Ko, &sample_plan(), None).into_string();
        assert!(!elsewhere.contains("제자리 복구입니다"));
    }

    /// PITR이면 요청 시각과 실제 도달 시각을 **둘 다** 보여준다.
    #[test]
    fn pitr_shows_both_the_requested_and_the_decided_moment() {
        let mut plan = sample_plan();
        plan.pitr = Some(PitrPoint {
            requested: "2026-06-12T13:00:00Z".to_string(),
            decided: "2026-06-12T12:59:47Z".to_string(),
            replay_slices: 3,
        });
        let out = plan_body(Lang::En, &plan, None).into_string();
        assert!(out.contains("2026-06-12T13:00:00Z"), "요청 시각이 없다");
        assert!(out.contains("2026-06-12T12:59:47Z"), "도달 시각이 없다");
        assert!(out.contains("slices to replay"));
        assert!(
            out.contains("That is normal"),
            "두 시각이 다른 이유를 설명하지 않는다"
        );
    }

    /// 백업 선택 복구에는 PITR 패널이 없다.
    #[test]
    fn a_plain_restore_has_no_point_in_time_panel() {
        let out = plan_body(Lang::En, &sample_plan(), None).into_string();
        assert!(!out.contains("slices to replay"));
    }

    #[test]
    fn conflicts_are_listed_when_present_and_silent_when_not() {
        let with = plan_body(Lang::Ko, &sample_plan(), None).into_string();
        assert!(with.contains("shop.orders"));

        let mut clean = sample_plan();
        clean.conflicting.clear();
        let without = plan_body(Lang::Ko, &clean, None).into_string();
        assert!(!without.contains("이미 데이터가 있습니다"));
    }

    #[test]
    fn hostile_strings_are_escaped() {
        let mut plan = sample_plan();
        plan.conflicting = vec!["<script>alert(1)</script>".to_string()];
        let out = plan_body(Lang::En, &plan, None).into_string();
        assert!(!out.contains("<script>alert"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn markup_carries_no_presentation() {
        let out = plan_body(Lang::Ko, &sample_plan(), None).into_string();
        assert!(!out.contains("style="));
    }

    /// 폼의 기본 상태는 **제자리 복구가 선택된 것**이다 — CLI 기본값과 같아야 한다.
    #[test]
    fn the_form_defaults_to_restoring_in_place() {
        let profiles = vec!["prod".to_string(), "dr".to_string()];
        let out = form_body(Lang::En, &profiles, &Prefill::default(), None).into_string();
        assert!(
            out.contains(r#"<option value="" selected>"#),
            "제자리 복구가 기본 선택이 아니다: {out}"
        );
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_AT)));
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_ONLY)));
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_FROM)));
    }

    /// 폼이 프리필을 되채운다(카탈로그에서 `?id=`로 넘어오는 경로).
    #[test]
    fn the_form_refills_what_was_submitted() {
        let prefill = Prefill {
            profile: Some("prod".to_string()),
            id: Some("bk-9".to_string()),
            at: Some("2026-01-01T00:00:00Z".to_string()),
            target_profile: Some("dr".to_string()),
            only: Some("shop.orders".to_string()),
        };
        let out = form_body(
            Lang::En,
            &["prod".to_string(), "dr".to_string()],
            &prefill,
            None,
        )
        .into_string();
        assert!(out.contains(r#"value="bk-9""#));
        assert!(out.contains(r#"value="2026-01-01T00:00:00Z""#));
        assert!(out.contains(r#"value="shop.orders""#));
        assert!(
            out.contains(r#"value="dr" selected"#),
            "대상이 유지되지 않았다"
        );
    }

    #[test]
    fn origin_labels_are_distinct() {
        assert_eq!(
            TargetOrigin::Profile("dr".to_string()).label(Lang::En),
            "--target-profile: dr"
        );
        assert_eq!(TargetOrigin::InPlace.label(Lang::En), "profile source");
        assert!(TargetOrigin::InPlace.is_in_place());
        assert!(!TargetOrigin::Profile("x".to_string()).is_in_place());
    }
}
