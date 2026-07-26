//! `migrate` 화면 마크업 — 폼 · dry-run 계획 · 확인.
//!
//! ## 계획이 답해야 하는 세 질문
//! `migrate`는 백업을 만들지 않는다 — source에서 target으로 **직접 복사**한다. 그래서
//! 실행 전에 운영자가 확인해야 하는 것이 `prune`과 다르다:
//!
//! 1. **어디에 연결되는가** — source·target의 토폴로지. 스탠드얼론인 줄 알았던 곳이 실은
//!    프로덕션 replica set이면 그 사실을 실행 전에 봐야 한다.
//! 2. **버전이 맞는가** — source/target 서버 버전과 CLI가 낸 비호환 경고.
//! 3. **무엇을 덮어쓰는가** — target에 이미 있는 네임스페이스(충돌). 이 목록이 비어 있지
//!    않으면 이 작업은 **남의 데이터 위에 쓰는 것**이다.
//!
//! 3번이 이 화면의 존재 이유다. `prune`은 "무엇이 사라지는가"를 보여주고, 여기서는
//! "무엇이 덮어써지는가"를 보여준다.
//!
//! ## 문서 수는 추정치라고 말한다
//! 계획의 네임스페이스별 문서 수는 자식이 준 **추정치**다(`MigratePlan::source_counts`).
//! 정확한 수처럼 보이게 그리면 운영자가 전송 후 숫자가 다를 때 실패로 오해한다 — 표 머리에
//! 그 사실을 적는다.

use maud::{html, Markup};

use crate::i18n::Lang;
use crate::web::routes::migrate as route;
use crate::web::view::components::{self, Level};

/// 계획 표의 네임스페이스 한 줄.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamespaceRow {
    /// `db.collection`.
    pub ns: String,
    /// source의 추정 문서 수.
    pub source: u64,
    /// target의 추정 문서 수(0이면 새로 생긴다).
    pub target: u64,
}

/// dry-run 계획.
#[derive(Debug, Clone)]
pub struct Plan {
    /// source 프로파일.
    pub profile: String,
    /// target 프로파일.
    pub target_profile: String,
    /// source 서버 버전.
    pub source_version: String,
    /// source 토폴로지 라벨.
    pub source_topology: String,
    /// target 서버 버전.
    pub target_version: String,
    /// 선택적 범위(`db` 또는 `db.collection`), 없으면 전체.
    pub scope: Option<String>,
    /// target에 이미 존재하는 네임스페이스 — **덮어쓰기 대상**.
    pub conflicting: Vec<String>,
    /// 버전 비호환 경고(자식이 만든 문장).
    pub version_warning: Option<String>,
    /// 네임스페이스별 규모.
    pub namespaces: Vec<NamespaceRow>,
    /// 이 계획의 지문 — 실행 요청에 그대로 실려 돌아온다.
    pub fingerprint: String,
}

impl Plan {
    /// 덮어쓸 것이 있는가.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicting.is_empty()
    }

    /// source 총 추정 문서 수.
    pub fn source_total(&self) -> u64 {
        self.namespaces.iter().map(|n| n.source).sum()
    }
}

/// `GET /migrate` — 폼.
pub fn form_body(
    lang: Lang,
    profiles: &[String],
    selected_source: Option<&str>,
    selected_target: Option<&str>,
    notice: Option<Markup>,
) -> Markup {
    html! {
        (components::page_head(route::MIGRATE_TITLE, Some(lang.sel(
            "Copy data straight from one profile's server to another's. This is not a backup — no manifest, no checksum, no PITR.",
            "한 프로파일의 서버에서 다른 프로파일의 서버로 데이터를 직접 복사합니다. 백업이 아닙니다 — manifest·체크섬·PITR가 만들어지지 않습니다.",
        ))))
        @if let Some(notice) = notice { (notice) }
        (components::panel(
            html! { h3 class="panel__title" { (lang.sel("Plan a migration", "마이그레이션 계획")) } },
            html! {
                form method="post" action=(route::MIGRATE_PLAN_PATH) {
                    (profile_select(lang, route::FIELD_PROFILE, "source profile", profiles, selected_source, lang.sel(
                        "Where the data is read from.",
                        "데이터를 읽어올 곳입니다.",
                    )))
                    (profile_select(lang, route::FIELD_TARGET_PROFILE, "target profile", profiles, selected_target, lang.sel(
                        "Where the data is written. Existing collections here can be overwritten.",
                        "데이터를 쓸 곳입니다. 여기 있는 기존 컬렉션이 덮어써질 수 있습니다.",
                    )))
                    div class="field" {
                        label class="field__label mono" for="f-db" { "db" }
                        input id="f-db" type="text" name=(route::FIELD_DB) autocomplete="off";
                        p class="field__hint" { (lang.sel(
                            "Optional — migrate only this database. Leave empty for everything.",
                            "선택 — 이 데이터베이스만 옮깁니다. 비우면 전체입니다.",
                        )) }
                    }
                    div class="field" {
                        label class="field__label mono" for="f-collection" { "collection" }
                        input id="f-collection" type="text" name=(route::FIELD_COLLECTION) autocomplete="off";
                        p class="field__hint" { (lang.sel(
                            "Optional — needs a db above. A collection name alone would match that name in every database.",
                            "선택 — 위의 db가 함께 있어야 합니다. 컬렉션 이름만 주면 모든 데이터베이스의 동명 컬렉션이 대상이 됩니다.",
                        )) }
                    }
                    button type="submit" { (lang.sel("Preview the migration", "마이그레이션 미리보기")) }
                }
            },
        ))
    }
}

/// 프로파일 선택 필드 하나.
fn profile_select(
    lang: Lang,
    field: &str,
    label: &str,
    profiles: &[String],
    selected: Option<&str>,
    hint: &str,
) -> Markup {
    html! {
        div class="field" {
            label class="field__label mono" for=(format!("f-{field}")) { (label) }
            select id=(format!("f-{field}")) name=(field) required {
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
            p class="field__hint" { (hint) }
        }
    }
}

/// dry-run 계획 화면 — 연결·버전·충돌 + (가드가 만든) 확인 블록.
pub fn plan_body(lang: Lang, plan: &Plan, confirm: Option<Markup>) -> Markup {
    html! {
        (components::page_head(route::MIGRATE_TITLE, Some(lang.sel(
            "Nothing has been copied yet — this is the plan.",
            "아직 아무것도 옮기지 않았습니다 — 이것은 계획입니다.",
        ))))
        (components::meta_list(&[
            ("source profile", plan.profile.clone()),
            ("source server", format!("{} ({})", plan.source_version, plan.source_topology)),
            ("target profile", plan.target_profile.clone()),
            ("target server", plan.target_version.clone()),
            ("scope", plan.scope.clone().unwrap_or_else(|| "(all databases)".to_string())),
            ("documents to copy", plan.source_total().to_string()),
        ]))

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
                lang.sel("The target already has data here", "target에 이미 데이터가 있습니다"),
                html! {
                    p { (lang.sel(
                        "These namespaces exist on the target. Migrating writes into them — with the drop option below, they are removed first; without it, documents are merged in and existing ones can be replaced.",
                        "아래 네임스페이스가 target에 이미 있습니다. 마이그레이션은 그 안에 씁니다 — 아래 drop 옵션을 켜면 먼저 지우고, 켜지 않으면 문서가 섞여 들어가며 기존 문서가 대체될 수 있습니다.",
                    )) }
                    ul class="plain" {
                        @for ns in &plan.conflicting {
                            li class="mono" { (ns) }
                        }
                    }
                },
            ))
        }

        (components::panel(
            html! {
                h3 class="panel__title" { (lang.sel("Namespaces", "네임스페이스")) }
                p class="field__hint" { (lang.sel(
                    "Document counts are estimates from the server — the number after the copy can differ.",
                    "문서 수는 서버가 준 추정치입니다 — 복사 후 숫자가 다를 수 있습니다.",
                )) }
            },
            html! {
                @if plan.namespaces.is_empty() {
                    p { (lang.sel("Nothing to copy in this scope.", "이 범위에 옮길 것이 없습니다.")) }
                } @else {
                    table class="grid" {
                        thead {
                            tr {
                                th { "namespace" }
                                th { (lang.sel("source (est.)", "source(추정)")) }
                                th { (lang.sel("target now (est.)", "현재 target(추정)")) }
                            }
                        }
                        tbody {
                            @for row in &plan.namespaces {
                                tr {
                                    td class="mono" { (row.ns) }
                                    td { (row.source) }
                                    td {
                                        (row.target)
                                        @if row.target > 0 {
                                            " " (components::badge(Level::Warn))
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            },
        ))

        @if let Some(confirm) = confirm { (confirm) }
        p { a href=(route::MIGRATE_PATH) { (lang.sel("Start over", "처음부터 다시")) } }
    }
}

/// 계획이 바뀌어 실행을 거부했을 때의 안내.
pub fn plan_changed_notice(lang: Lang) -> Markup {
    components::notice(
        Level::Fail,
        lang.sel(
            "The plan changed — nothing was copied",
            "계획이 바뀌었습니다 — 아무것도 옮기지 않았습니다",
        ),
        html! {
            p { (lang.sel(
                "Between the preview and your approval, one of these servers changed — a namespace appeared, or its size moved. The plan you approved is no longer the plan that would run, so nothing was copied.",
                "미리보기와 승인 사이에 두 서버 중 한쪽이 바뀌었습니다 — 네임스페이스가 생겼거나 규모가 달라졌습니다. 승인한 계획과 지금 실행될 계획이 다르므로 아무것도 옮기지 않았습니다.",
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
            "No migrate job with that id is in this console's history.",
            "그 id의 migrate 잡이 이 콘솔 이력에 없습니다.",
        )) } },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_plan() -> Plan {
        Plan {
            profile: "prod".to_string(),
            target_profile: "dr".to_string(),
            source_version: "7.0.5".to_string(),
            source_topology: "replicaSet".to_string(),
            target_version: "6.0.9".to_string(),
            scope: None,
            conflicting: vec!["shop.orders".to_string()],
            version_warning: Some(
                "source 7.0 → target 6.0은 하위 호환이 보장되지 않습니다".to_string(),
            ),
            namespaces: vec![
                NamespaceRow {
                    ns: "shop.orders".to_string(),
                    source: 1200,
                    target: 340,
                },
                NamespaceRow {
                    ns: "shop.users".to_string(),
                    source: 80,
                    target: 0,
                },
            ],
            fingerprint: "fp-1".to_string(),
        }
    }

    /// **done_criteria**: 계획에 연결·버전·충돌 네임스페이스가 전부 표시된다.
    #[test]
    fn the_plan_shows_connection_versions_and_conflicts() {
        let out = plan_body(Lang::En, &sample_plan(), None).into_string();

        // 연결
        assert!(out.contains("replicaSet"), "source 토폴로지가 없다");
        assert!(
            out.contains("prod") && out.contains("dr"),
            "양쪽 프로파일이 없다"
        );
        // 버전
        assert!(out.contains("7.0.5"), "source 버전이 없다");
        assert!(out.contains("6.0.9"), "target 버전이 없다");
        assert!(out.contains("하위 호환"), "버전 경고가 없다");
        // 충돌
        assert!(out.contains("shop.orders"), "충돌 네임스페이스가 없다");
    }

    /// 충돌이 없으면 덮어쓰기 배너를 띄우지 않는다 — 매번 뜨는 빨간 배너는 곧 무시된다.
    #[test]
    fn no_conflicts_means_no_overwrite_banner() {
        let mut plan = sample_plan();
        plan.conflicting.clear();
        plan.namespaces.iter_mut().for_each(|n| n.target = 0);
        let out = plan_body(Lang::Ko, &plan, None).into_string();
        assert!(
            !out.contains("이미 데이터가 있습니다"),
            "충돌이 없는데 배너가 떴다"
        );
    }

    /// 버전 경고가 없으면 경고 배너도 없다.
    #[test]
    fn no_version_warning_means_no_version_banner() {
        let mut plan = sample_plan();
        plan.version_warning = None;
        let out = plan_body(Lang::Ko, &plan, None).into_string();
        assert!(!out.contains("호환되지 않을 수 있습니다"));
    }

    #[test]
    fn source_total_sums_namespaces() {
        assert_eq!(sample_plan().source_total(), 1280);
    }

    #[test]
    fn confirm_block_is_attached_when_given() {
        let confirm = html! { form id="the-confirm-form" {} };
        let out = plan_body(Lang::En, &sample_plan(), Some(confirm)).into_string();
        assert!(out.contains("the-confirm-form"));
    }

    #[test]
    fn hostile_namespace_names_are_escaped() {
        let mut plan = sample_plan();
        plan.conflicting = vec!["<script>alert(1)</script>".to_string()];
        let out = plan_body(Lang::En, &plan, None).into_string();
        assert!(!out.contains("<script>alert"));
        assert!(out.contains("&lt;script&gt;"));
    }

    #[test]
    fn markup_carries_no_presentation() {
        let out = plan_body(Lang::Ko, &sample_plan(), None).into_string();
        assert!(!out.contains("style="), "인라인 스타일이 있다");
    }

    #[test]
    fn form_offers_both_profile_selects() {
        let profiles = vec!["prod".to_string(), "dr".to_string()];
        let out = form_body(Lang::En, &profiles, Some("prod"), Some("dr"), None).into_string();
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_PROFILE)));
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_TARGET_PROFILE)));
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_DB)));
        assert!(out.contains(&format!(r#"name="{}""#, route::FIELD_COLLECTION)));
    }

    /// 이 화면은 백업이 아니라는 것을 폼에서부터 말한다 — 운영자가 두 명령을 혼동하면
    /// manifest 없는 복사를 백업으로 착각한다.
    #[test]
    fn the_form_says_this_is_not_a_backup() {
        let out = form_body(Lang::Ko, &[], None, None, None).into_string();
        assert!(
            out.contains("백업이 아닙니다"),
            "백업이 아니라는 고지가 없다"
        );
    }
}
