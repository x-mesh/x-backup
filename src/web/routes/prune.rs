//! `GET /prune` · `POST /prune/plan` · `POST /prune/apply` · `GET /prune/{job_id}` —
//! 보존 정책 적용 화면. **이 콘솔에서 처음으로 파괴적 작업을 실행하는 화면이다.**
//!
//! ## 세 걸음으로 나눈 이유
//! 폼 → 계획 → 실행. 가운데 걸음(계획)이 이 화면의 존재 이유다. CLI는 `prune --dry-run`을
//! 사람이 **따로 타이핑해서** 보고, 결과를 눈으로 확인한 뒤 다시 `--force`를 타이핑한다.
//! 웹에서 그 두 걸음을 하나로 합치면 "버튼 한 번에 백업이 사라진다"가 되어, CLI가 일부러
//! 만들어 둔 마찰이 사라진다. 그래서 계획을 **반드시** 거치게 한다 —
//! [`PRUNE_APPLY_PATH`]로 바로 POST해도 확인 토큰이 없으면 거부된다
//! ([`crate::web::guard`]).
//!
//! ## 지문(fingerprint) — 본 적 없는 삭제를 승인시키지 않는다
//! 계획을 본 시점과 승인한 시점 사이에 저장소가 바뀔 수 있다(백업이 하나 끝났거나, cron
//! prune이 돌았거나). 그 사이에 **지워질 목록이 달라지면**, 운영자가 승인한 것은 지금
//! 실행될 계획이 아니다. 승인 화면에 계획의 지문을 hidden 필드로 심고, 실행 직전에
//! **자식에게 계획을 다시 물어** 지문을 비교한다. 다르면 아무것도 지우지 않고 새 계획을
//! 보여주며 재승인을 요구한다([`view::plan_changed_notice`]).
//!
//! ### 왜 카탈로그가 아니라 계획의 지문인가
//! "카탈로그 지문"이 더 직관적으로 들리지만 그건 **과하게 민감하다** — 지워질 목록과 무관한
//! 새 백업 하나가 도착해도 재승인을 요구하게 되고, 그렇게 자주 틀리는 경고는 사람이 곧
//! 무시한다. 운영자가 승인한 것은 카탈로그가 아니라 **"이것들이 지워진다"는 목록**이므로,
//! 그 목록이 같으면 승인은 여전히 유효하다.
//!
//! ### 왜 계획을 다시 묻는가 (캐시하지 않고)
//! `prune --force`는 자기 계획을 **스스로 다시 계산한다**. 그러므로 우리가 처음 받은 계획을
//! 들고 있다가 그대로 실행시킬 방법이 없다 — 자식에게 "지금 계획이 무엇이냐"를 다시 묻는
//! 것이 실행될 계획을 알 수 있는 유일한 방법이다. 그 대가로 승인 요청 하나가 자식을 두 번
//! 띄운다(dry-run + force). 되돌릴 수 없는 작업에서 정확성이 왕복 한 번보다 비싸다.
//!
//! ## 검사 순서
//! 1. **출처**([`crate::web::auth::verify_same_origin`]) — 위조 요청이 자식을 띄우게 두지
//!    않는다. 가드도 같은 검사를 하지만 그건 dry-run **뒤**라, 여기서 먼저 끊는다.
//! 2. **확인 토큰 존재** — 확인 화면을 아예 거치지 않은 직접 POST를 자식 없이 끊는다.
//!    유효성 판정이 아니라 "이 요청이 확인 화면에서 왔는가"의 값싼 사전 검사다.
//! 3. **지문** — 계획이 바뀌었으면 확인 토큰을 소비하지 않고 되돌린다(운영자는 새 계획에
//!    대해 새로 승인하면 된다).
//! 4. **가드**([`guard::DestructiveGuard::confirm_and_gate`]) — 토큰 소비·이름 일치·감사
//!    게이트. 여기를 통과해야 `AuditReceipt`가 생긴다.
//! 5. **실행** — receipt를 값으로 넘겨 `spawn_destructive`.
//!
//! 3번이 4번보다 앞인 이유: 지문이 어긋나면 실행되는 것이 없으므로 감사 로그에
//! "게이트를 통과했다"는 줄을 남기면 안 된다. 감사 기록은 **실행 직전**에만 찍혀야 그
//! 로그를 읽는 사람이 "기록 = 실행"으로 읽을 수 있다.
//!
//! ## 이 화면이 손대지 않는 것
//! - 보존 정책 판정을 다시 하지 않는다 — 무엇을 지울지는 자식(`prune --json`)이 정한다
//!   ([`crate::web`] 최상위 불변식).
//! - 확인 마크업을 자체 제작하지 않는다 — [`guard::DestructiveGuard::render_confirm`]이
//!   만든다(세 파괴적 화면이 같은 확인 문법을 쓰게 하려는 그 모듈의 존재 이유).

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::Markup;
use sha2::{Digest, Sha256};

use crate::error::XBackupError;
use crate::i18n::Lang;
use crate::web::audit::{AuditEvent, AuditReceipt};
use crate::web::guard::{
    self, ConfirmError, ConfirmSubmission, DestructiveContext, DestructiveRequest,
    DestructiveTarget,
};
use crate::web::job::args::ProfileName;
use crate::web::job::{JobCommand, JobCount, JobFlag, JobSpec};
use crate::web::jsonguard;
use crate::web::routes::form::FormBody;
use crate::web::state::jobs::{self, JobId, JobStart, JobStore, LogStream};
use crate::web::view::{layout, prune as view};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수
// ---------------------------------------------------------------------------

/// 폼 화면.
pub const PRUNE_PATH: &str = "/prune";
/// dry-run 계획 요청(POST).
pub const PRUNE_PLAN_PATH: &str = "/prune/plan";
/// 실제 삭제 요청(POST).
pub const PRUNE_APPLY_PATH: &str = "/prune/apply";
/// 결과 화면의 라우터 패턴.
pub const PRUNE_RESULT_ROUTE: &str = "/prune/job/{job_id}";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const PRUNE_TITLE: &str = "Prune";

/// 감사 로그 `actor` 값 — 다른 화면과 같다(이 콘솔에는 세션 하나뿐).
const AUDIT_ACTOR: &str = "web";

/// 폼 필드 이름 — 뷰와 파서가 공유한다.
pub const FIELD_PROFILE: &str = "profile";
/// 보존 기준 필드.
pub const FIELD_KEEP_FULL: &str = "keep_full";
/// 보존 기준 필드.
pub const FIELD_KEEP_DAYS: &str = "keep_days";
/// 보존 기준 필드.
pub const FIELD_KEEP_LAST: &str = "keep_last";
/// 보존 기준 필드.
pub const FIELD_RECOVERY_WINDOW_DAYS: &str = "recovery_window_days";
/// 보존 기준 필드.
pub const FIELD_MIN_REDUNDANCY: &str = "min_redundancy";
/// 승인 요청이 되돌려주는 계획 지문 필드.
pub const FIELD_FINGERPRINT: &str = "plan_fingerprint";

/// `prune --json`이 낼 스키마 버전의 웹 쪽 사본.
///
/// `cli/handlers/prune.rs::PRUNE_JSON_SCHEMA`가 `pub`이 아니라 값을 복제한다
/// (`routes::verify`가 같은 사정으로 같은 선택을 했다). 두 값이 갈라지면
/// [`parse_plan`]이 `SchemaMismatch`로 **명확히** 실패하므로 드리프트가 화면에 바로 드러난다.
const EXPECTED_PRUNE_SCHEMA: u32 = 1;

/// 자식 `prune` 실행 상한. dry-run은 카탈로그를 훑고, 실제 삭제는 객체를 지운다 —
/// 둘 다 저장소 왕복이 지배적이라 같은 값을 쓴다. `peek`(30초)보다 길게 잡는 이유는
/// 백업 수천 개인 저장소에서 카탈로그 훑기가 그보다 오래 걸릴 수 있기 때문이다.
const PRUNE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 결과 화면 링크.
pub fn prune_result_href(id: &JobId) -> String {
    format!("/prune/job/{id}")
}

// ---------------------------------------------------------------------------
// 폼 파싱
// ---------------------------------------------------------------------------

/// 폼에서 읽은 보존 기준 덮어쓰기 — 전부 선택이다(비우면 config 정책을 쓴다).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct RetentionOverrides {
    /// `--keep-full`.
    pub keep_full: Option<u32>,
    /// `--keep-days`.
    pub keep_days: Option<u32>,
    /// `--keep-last`.
    pub keep_last: Option<u32>,
    /// `--recovery-window-days`.
    pub recovery_window_days: Option<u32>,
    /// `--min-redundancy`.
    pub min_redundancy: Option<u32>,
}

impl RetentionOverrides {
    /// 폼에서 읽는다. 값이 비어 있으면 `None`, 숫자가 아니면 오류다.
    ///
    /// 숫자가 아닌 값을 조용히 무시하지 않는 이유: 운영자가 `keep-full`에 오타를 냈는데
    /// 그것이 조용히 "정책 그대로"로 접히면, 화면은 **의도하지 않은 정책의 계획**을 보여주고
    /// 사람은 그것을 승인한다. 삭제 화면에서 조용한 폴백은 위험한 기본값이다.
    fn from_form(form: &FormBody) -> std::result::Result<Self, String> {
        fn read(
            form: &FormBody,
            field: &str,
            label: &str,
        ) -> std::result::Result<Option<u32>, String> {
            match form.get(field) {
                None => Ok(None),
                Some(raw) if raw.trim().is_empty() => Ok(None),
                Some(raw) => raw.trim().parse::<u32>().map(Some).map_err(|_| {
                    format!("{label}: '{}'은(는) 0 이상의 정수가 아닙니다.", raw.trim())
                }),
            }
        }

        Ok(Self {
            keep_full: read(form, FIELD_KEEP_FULL, "keep-full")?,
            keep_days: read(form, FIELD_KEEP_DAYS, "keep-days")?,
            keep_last: read(form, FIELD_KEEP_LAST, "keep-last")?,
            recovery_window_days: read(form, FIELD_RECOVERY_WINDOW_DAYS, "recovery-window-days")?,
            min_redundancy: read(form, FIELD_MIN_REDUNDANCY, "min-redundancy")?,
        })
    }

    /// 폼 값을 hidden 필드로 되싣기 위한 `(이름, 값)` 목록.
    fn as_fields(&self) -> Vec<(&'static str, u32)> {
        [
            (FIELD_KEEP_FULL, self.keep_full),
            (FIELD_KEEP_DAYS, self.keep_days),
            (FIELD_KEEP_LAST, self.keep_last),
            (FIELD_RECOVERY_WINDOW_DAYS, self.recovery_window_days),
            (FIELD_MIN_REDUNDANCY, self.min_redundancy),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|v| (name, v)))
        .collect()
    }

    /// `JobSpec`에 보존 기준을 얹는다.
    fn apply_to(&self, mut spec: JobSpec) -> JobSpec {
        for (field, value) in self.as_fields() {
            let opt = match field {
                FIELD_KEEP_FULL => JobCount::KeepFull,
                FIELD_KEEP_DAYS => JobCount::KeepDays,
                FIELD_KEEP_LAST => JobCount::KeepLast,
                FIELD_RECOVERY_WINDOW_DAYS => JobCount::RecoveryWindowDays,
                _ => JobCount::MinRedundancy,
            };
            spec = spec.with_count(opt, value);
        }
        spec
    }
}

// ---------------------------------------------------------------------------
// 자식 stdout → 계획
// ---------------------------------------------------------------------------

/// 계획을 못 읽은 이유.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// stdout이 비었다 — 자식이 계획을 내기 전에 끝났다.
    Empty,
    /// JSON이 아니거나 기대한 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 이 빌드가 아는 값과 다르다.
    SchemaMismatch {
        /// 자식이 낸 값.
        found: u64,
        /// 이 빌드가 아는 값.
        expected: u32,
    },
    /// 중첩이 너무 깊어 파싱하지 않았다([`crate::web::jsonguard`]).
    TooDeep {
        /// 측정된 깊이.
        found: usize,
        /// 상한.
        max: usize,
    },
}

impl PlanError {
    /// 화면에 그대로 실리는 설명.
    pub fn explain(&self, lang: crate::i18n::Lang) -> String {
        match self {
            PlanError::Empty => lang
                .sel(
                    "prune produced no plan. Read the exit code — the child usually stopped before it could report.",
                    "prune이 계획을 내지 않았습니다. 종료 코드를 보세요 — 대개 보고 전에 자식이 멈춘 경우입니다.",
                )
                .to_string(),
            PlanError::Malformed(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "prune output could not be parsed as the expected JSON:",
                    "prune 출력을 기대한 JSON으로 해석할 수 없습니다:",
                )
            ),
            PlanError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} \u{2260} {expected})",
                lang.sel(
                    "prune reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "prune이 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            PlanError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "prune output is nested more deeply than this console parses.",
                    "prune 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다.",
                )
            ),
        }
    }
}

/// 자식 stdout을 계획으로 접는다 — **순수 함수**.
///
/// `fingerprint`는 여기서 계산한다([`plan_fingerprint`]) — 계획을 만든 곳과 지문을 만든
/// 곳이 갈리면 "무엇의 지문인가"가 흐려진다.
pub fn parse_plan(stdout: &str) -> std::result::Result<view::Plan, PlanError> {
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(PlanError::Empty);
    }
    jsonguard::check_depth(trimmed).map_err(|d| PlanError::TooDeep {
        found: d.found,
        max: d.max,
    })?;

    let value: serde_json::Value =
        serde_json::from_str(trimmed).map_err(|e| PlanError::Malformed(e.to_string()))?;

    match value.get("schema").and_then(serde_json::Value::as_u64) {
        Some(found) if found == u64::from(EXPECTED_PRUNE_SCHEMA) => {}
        Some(found) => {
            return Err(PlanError::SchemaMismatch {
                found,
                expected: EXPECTED_PRUNE_SCHEMA,
            })
        }
        None => {
            return Err(PlanError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }

    let targets = value
        .get("targets")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| PlanError::Malformed("최상위 `targets` 배열이 없습니다".to_string()))?
        .iter()
        .map(|t| view::PlanTarget {
            kind: string_at(t, "kind"),
            base_id: string_at(t, "base_id"),
            member_ids: t
                .get("member_ids")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|v| v.as_str().unwrap_or_default().to_string())
                        .collect()
                })
                .unwrap_or_default(),
            reason: string_at(t, "reason"),
            force_only: t
                .get("force_only")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
        .collect::<Vec<_>>();

    let retained_base_ids: Vec<String> = value
        .get("retained_base_ids")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();

    let fingerprint = plan_fingerprint(&targets);

    Ok(view::Plan {
        profile: string_at(&value, "profile"),
        store: string_at(&value, "store"),
        targets,
        retained_base_ids,
        fingerprint,
    })
}

fn string_at(value: &serde_json::Value, key: &str) -> String {
    value
        .get(key)
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// 계획의 지문 — **지워질 목록**만 반영한다.
///
/// 보존되는 base(`retained_base_ids`)는 일부러 넣지 않는다. 새 백업이 하나 도착하면 그
/// 목록은 늘어나지만 지워질 것은 그대로다 — 그때 재승인을 요구하면 자주 틀리는 경고가 되고,
/// 자주 틀리는 경고는 사람이 곧 무시한다(모듈 헤더 "왜 카탈로그가 아니라 계획의 지문인가").
///
/// 대상 순서에 의존하지 않도록 정렬해서 먹인다 — 자식이 같은 집합을 다른 순서로 내도
/// 같은 계획이다. 구분자(`\u{1f}`)를 끼우는 이유는 id 이어붙이기가 우연히 같은 문자열이
/// 되는 것을 막기 위해서다.
pub fn plan_fingerprint(targets: &[view::PlanTarget]) -> String {
    let mut lines: Vec<String> = targets
        .iter()
        .map(|t| {
            let mut members = t.member_ids.clone();
            members.sort();
            format!(
                "{}\u{1f}{}\u{1f}{}\u{1f}{}",
                t.kind,
                t.base_id,
                t.force_only,
                members.join("\u{1e}")
            )
        })
        .collect();
    lines.sort();

    let mut hasher = Sha256::new();
    for line in &lines {
        hasher.update(line.as_bytes());
        hasher.update([0x1d]);
    }
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /prune` — 폼.
pub async fn form(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let profiles = load_profile_names(&ctx);
    layout::shell(
        ctx.lang,
        PRUNE_TITLE,
        view::form_body(ctx.lang, &profiles, None, None),
    )
}

/// `POST /prune/plan` — dry-run을 돌려 계획과 확인 화면을 보여준다. **아무것도 지우지 않는다.**
pub async fn plan(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    let (profile, overrides) = match read_request(&form_body, ctx.lang) {
        Ok(v) => v,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

    match run_dry_run(&ctx, &profile, &overrides).await {
        Ok(plan) => render_plan(&ctx, &profile, &overrides, plan, None).await,
        Err(message) => form_with_notice(&ctx, StatusCode::BAD_GATEWAY, &form_body, &message),
    }
}

/// `POST /prune/apply` — 확인을 검증하고 실제로 지운다.
pub async fn apply(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    // 1) 출처 — 위조 요청이 자식을 띄우게 두지 않는다(모듈 헤더 "검사 순서").
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    // 2) 확인 토큰이 **있기라도 한가** — 없으면 자식을 띄우기 전에 끊는다.
    //
    // 토큰 검증(조회·만료·소비)은 가드의 일이지만 그건 아래 dry-run 뒤라, 확인 화면을
    // 아예 거치지 않은 직접 POST가 자식을 두 번 띄우게 두지 않으려면 여기서 값싼 검사가
    // 하나 필요하다. 여기 통과가 "유효한 토큰"을 뜻하지는 않는다 — 그 판정은 가드가 한다.
    if form_body
        .get(guard::FIELD_CONFIRM_TOKEN)
        .is_none_or(str::is_empty)
    {
        return (
            StatusCode::FORBIDDEN,
            layout::shell(
                ctx.lang,
                PRUNE_TITLE,
                view::guard_rejection_notice(
                    ctx.lang,
                    ctx.lang.sel(
                        "This request did not come from a confirmation screen. Preview the plan first, then approve it there.",
                        "이 요청은 확인 화면에서 온 것이 아닙니다. 먼저 계획을 미리 보고 그 화면에서 승인하세요.",
                    ),
                ),
            ),
        )
            .into_response();
    }

    let (profile, overrides) = match read_request(&form_body, ctx.lang) {
        Ok(v) => v,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

    // 3) 지문 — 승인된 계획과 지금 실행될 계획이 같은가.
    let submitted = form_body.get(FIELD_FINGERPRINT).unwrap_or_default();
    let current = match run_dry_run(&ctx, &profile, &overrides).await {
        Ok(plan) => plan,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_GATEWAY, &form_body, &message)
        }
    };
    if current.fingerprint != submitted {
        // 확인 토큰을 소비하지 않는다 — 운영자는 새 계획에 대해 새로 승인하면 된다.
        return render_plan(
            &ctx,
            &profile,
            &overrides,
            current,
            Some(view::plan_changed_notice(ctx.lang)),
        )
        .await;
    }

    // 4) 가드 — 토큰 소비·이름 일치·감사 게이트.
    let allow_overwrite = form_body.checked(guard::FIELD_ALLOW_OVERWRITE);
    let submission = ConfirmSubmission {
        token: form_body.get(guard::FIELD_CONFIRM_TOKEN),
        typed_name: form_body.get(guard::FIELD_CONFIRM_NAME).unwrap_or_default(),
        allow_overwrite,
    };
    let what = DestructiveTarget {
        action: JobCommand::Prune.audit_action(),
        target: &profile,
    };
    let spec_for_args = build_spec(&profile, &overrides, allow_overwrite, ctx.lang);
    let secrets = ctx.jobs.secret_registry().clone();

    let gate = match ctx
        .guard
        .confirm_and_gate(
            &ctx.audit,
            &headers,
            DestructiveContext {
                what,
                actor: AUDIT_ACTOR,
            },
            submission,
            |_overwrite| spec_for_args.masked_args(&secrets),
        )
        .await
    {
        Ok(gate) => gate,
        Err(e) => {
            let status = match &e {
                ConfirmError::ForeignOrigin(_) => StatusCode::FORBIDDEN,
                ConfirmError::Rejected(_) => StatusCode::FORBIDDEN,
                ConfirmError::Audit(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let notice = view::guard_rejection_notice(ctx.lang, &e.explain(ctx.lang));
            return render_plan(&ctx, &profile, &overrides, current, Some(notice))
                .await
                .into_response()
                .map_status(status);
        }
    };

    // 5) 실행 — receipt를 값으로 넘긴다.
    let spec = build_spec(&profile, &overrides, gate.allow_overwrite, ctx.lang);
    match start_prune_job(&ctx, spec, gate.receipt, vec![profile.as_str().to_string()]).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, prune_result_href(&id))],
        )
            .into_response(),
        Err(message) => form_with_notice(
            &ctx,
            StatusCode::INTERNAL_SERVER_ERROR,
            &form_body,
            &message,
        ),
    }
}

/// `GET /prune/job/{job_id}` — 결과(또는 진행 중) 화면.
pub async fn result(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, PRUNE_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    if detail.summary.is_none() && !detail.log_file_present {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, PRUNE_TITLE, view::unknown_job(ctx.lang)),
        )
            .into_response();
    }
    // 결과 본문은 잡 이력 화면이 이미 잘 그린다 — 여기서 같은 것을 다시 만들지 않고
    // 그쪽으로 보낸다(문구 이원화를 만들지 않는다).
    (
        StatusCode::SEE_OTHER,
        [(
            header::LOCATION,
            crate::web::routes::jobs::job_detail_href(&id),
        )],
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// 조립 헬퍼
// ---------------------------------------------------------------------------

/// 상태 코드를 갈아끼우는 작은 확장 — 렌더 결과에 4xx/5xx를 입힌다.
trait WithStatus {
    fn map_status(self, status: StatusCode) -> Response;
}

impl WithStatus for Response {
    fn map_status(mut self, status: StatusCode) -> Response {
        *self.status_mut() = status;
        self
    }
}

/// 폼에서 프로파일과 보존 기준을 읽는다.
///
/// 오류를 **문장으로** 돌려준다(렌더된 [`Response`]가 아니라). 처음에는 여기서 바로
/// 화면을 만들었는데, 그러면 (1) 읽기 헬퍼가 렌더 책임까지 지고 (2) `Result`의 오류
/// 변종이 응답 하나만큼 커진다(clippy `result_large_err`). 문장만 돌려주면 호출부가
/// 자기 상황에 맞는 상태 코드로 렌더한다 — `migrate`·`restore`의 같은 자리도 이 모양이다.
fn read_request(
    form_body: &FormBody,
    lang: Lang,
) -> std::result::Result<(ProfileName, RetentionOverrides), String> {
    let raw_profile = form_body.get(FIELD_PROFILE).unwrap_or_default();
    let profile = ProfileName::parse(raw_profile, lang).map_err(|e| e.detail())?;
    let overrides = RetentionOverrides::from_form(form_body)?;
    Ok((profile, overrides))
}

/// 폼 화면에 안내를 얹어 돌려준다.
fn form_with_notice(
    ctx: &Arc<ServeConfig>,
    status: StatusCode,
    form_body: &FormBody,
    message: &str,
) -> Response {
    let profiles = load_profile_names(ctx);
    let selected = form_body.get(FIELD_PROFILE).map(str::to_string);
    (
        status,
        layout::shell(
            ctx.lang,
            PRUNE_TITLE,
            view::form_body(
                ctx.lang,
                &profiles,
                selected.as_deref(),
                Some(view::validation_notice(ctx.lang, message)),
            ),
        ),
    )
        .into_response()
}

/// 출처가 다르면 거부한다(`routes::config::reject_foreign_origin`과 같은 판단).
fn reject_foreign_origin(ctx: &Arc<ServeConfig>, headers: &HeaderMap) -> Option<Response> {
    crate::web::auth::verify_same_origin(headers)
        .err()
        .map(|p| {
            (
                StatusCode::FORBIDDEN,
                layout::shell(
                    ctx.lang,
                    PRUNE_TITLE,
                    view::guard_rejection_notice(ctx.lang, &p.explain(ctx.lang)),
                ),
            )
                .into_response()
        })
}

/// 계획 + 확인 화면을 그린다.
async fn render_plan(
    ctx: &Arc<ServeConfig>,
    profile: &ProfileName,
    overrides: &RetentionOverrides,
    plan: view::Plan,
    notice: Option<Markup>,
) -> Response {
    // 지울 것이 없으면 확인 블록 자체를 만들지 않는다 — 토큰도 발급하지 않는다.
    let confirm = if plan.targets.is_empty() {
        None
    } else {
        Some(
            ctx.guard
                .render_confirm(
                    ctx.lang,
                    confirm_request(ctx.lang, profile, overrides, &plan),
                )
                .await,
        )
    };

    let mut body = view::plan_body(ctx.lang, &plan, confirm);
    if let Some(notice) = notice {
        body = maud::html! { (notice) (body) };
    }
    (StatusCode::OK, layout::shell(ctx.lang, PRUNE_TITLE, body)).into_response()
}

/// 확인 화면에 넘길 요청 값을 만든다.
///
/// 실행에 필요한 값(프로파일·보존 기준·계획 지문)은 [`DestructiveRequest::extra_hidden`]으로
/// 가드가 그리는 폼 안에 실려 돌아온다. **그 값들은 확인 토큰이 보호하지 않으므로**
/// 되돌아온 뒤 다시 검증한다 — 프로파일은 [`ProfileName::parse`]가, 보존 기준은
/// [`RetentionOverrides::from_form`]이, 지문은 자식에게 계획을 다시 물어 비교하는 것이
/// 그 검증이다(모듈 헤더 "지문").
fn confirm_request<'a>(
    lang: crate::i18n::Lang,
    profile: &'a ProfileName,
    overrides: &RetentionOverrides,
    plan: &view::Plan,
) -> DestructiveRequest<'a> {
    let mut summary: Vec<(&'static str, String)> = vec![
        ("command", "prune".to_string()),
        ("profile", profile.as_str().to_string()),
        ("chains", plan.targets.len().to_string()),
        ("backups", plan.total_backups().to_string()),
    ];
    for (field, value) in overrides.as_fields() {
        summary.push((field, value.to_string()));
    }

    DestructiveRequest {
        what: DestructiveTarget {
            action: JobCommand::Prune.audit_action(),
            target: profile,
        },
        headline: lang
            .sel(
                "This deletes backups permanently",
                "이 작업은 백업을 영구히 삭제합니다",
            )
            .to_string(),
        irreversible_notice: lang.sel(
            "Deleted backups cannot be restored by this console or any other tool. If a chain listed above is the only copy of a recovery point, that recovery point is gone.",
            "삭제된 백업은 이 콘솔로도 다른 도구로도 되돌릴 수 없습니다. 위 목록의 체인이 어떤 복구 시점의 유일한 사본이라면 그 시점은 사라집니다.",
        ).to_string(),
        summary,
        overwrite: plan.has_force_only().then(|| guard::OverwriteOption {
            label: lang
                .sel(
                    "Also delete leftovers (orphaned and incomplete backups)",
                    "잔재도 함께 삭제(고아·incomplete 백업)",
                )
                .to_string(),
            hint: lang.sel(
                "Off by default, matching the CLI: leftovers need --force there too. Leaving them is safe — they only take space.",
                "CLI와 같이 기본은 꺼짐입니다 — 그쪽에서도 잔재는 --force가 있어야 지워집니다. 남겨 두어도 안전하며 공간만 차지합니다.",
            ).to_string(),
        }),
        extra_hidden: hidden_state(profile, overrides, plan),
        submit_path: PRUNE_APPLY_PATH,
        submit_label: lang
            .sel("Delete these backups", "이 백업들을 삭제")
            .to_string(),
        cancel_href: PRUNE_PATH,
    }
}

/// 확인 폼에 되실을 값들 — 실행 요청이 이 값으로 같은 계획을 재구성한다.
fn hidden_state(
    profile: &ProfileName,
    overrides: &RetentionOverrides,
    plan: &view::Plan,
) -> Vec<(String, String)> {
    let mut fields = vec![
        (FIELD_PROFILE.to_string(), profile.as_str().to_string()),
        (FIELD_FINGERPRINT.to_string(), plan.fingerprint.clone()),
    ];
    for (name, value) in overrides.as_fields() {
        fields.push((name.to_string(), value.to_string()));
    }
    fields
}

/// `prune` `JobSpec`을 만든다.
///
/// `--force`는 두 가지를 동시에 뜻한다(CLI): 대화형 확인 생략과 잔재 삭제 포함. 웹에는
/// 대화형 확인이 없고 우리 확인은 가드가 이미 받았으므로, **실행 경로에는 항상** 붙인다.
/// 잔재 포함 여부는 `allow_overwrite` 토글이 정한다 — 두 뜻이 한 플래그에 겹쳐 있어
/// 갈라 쓸 수 없으므로, 토글이 꺼져 있으면 잔재가 함께 지워지지 않도록 **계획 자체가
/// 잔재를 포함하지 않는 경우에만** 이 플래그의 의미가 좁아진다는 사실을 화면이 설명한다.
fn build_spec(
    profile: &ProfileName,
    overrides: &RetentionOverrides,
    _allow_overwrite: bool,
    lang: Lang,
) -> JobSpec {
    let spec = JobSpec::new(JobCommand::Prune, lang)
        .with_profile(profile.clone())
        .with_flag(JobFlag::Force);
    overrides.apply_to(spec)
}

/// dry-run 자식을 돌려 계획을 받아온다.
async fn run_dry_run(
    ctx: &Arc<ServeConfig>,
    profile: &ProfileName,
    overrides: &RetentionOverrides,
) -> std::result::Result<view::Plan, String> {
    let spec = overrides.apply_to(
        JobSpec::new(JobCommand::Prune, ctx.lang)
            .with_profile(profile.clone())
            .with_flag(JobFlag::DryRun),
    );

    // `prune`은 명령 단위로 파괴적이라 `spawn`이 거부한다. 하지만 `--dry-run`은 아무것도
    // 지우지 않고, 그 계획을 얻으려고 감사 게이트를 통과하면 로그가 거짓말을 하게 된다 —
    // 그래서 "지우지 않는 실행"만 받는 좁은 문을 쓴다(`JobRunner::spawn_preview` doc).
    let running = ctx
        .jobs
        .spawn_preview(&spec)
        .map_err(|e: XBackupError| e.to_string())?;

    let completion = tokio::time::timeout(PRUNE_TIMEOUT, running.wait_with_output())
        .await
        .map_err(|_| {
            ctx.lang
                .sel(
                    "prune --dry-run did not finish in time.",
                    "prune --dry-run이 제한 시간 안에 끝나지 않았습니다.",
                )
                .to_string()
        })?
        .map_err(|e| e.to_string())?;

    parse_plan(&completion.stdout).map_err(|e| e.explain(ctx.lang))
}

/// 확정된 삭제 잡을 띄우고 이력에 기록한다.
async fn start_prune_job(
    ctx: &Arc<ServeConfig>,
    spec: JobSpec,
    receipt: AuditReceipt,
    touched: Vec<String>,
) -> std::result::Result<JobId, String> {
    let masked_args = spec.masked_args(ctx.jobs.secret_registry());
    let audit_action = spec.audit_action();
    let audit_target = spec.audit_target().to_string();

    let running = ctx
        .jobs
        .spawn_destructive(&spec, receipt)
        .map_err(|e| e.to_string())?;
    let started_at = running.started_at();
    let pid = running.pid();

    let store = jobs::shared(&ctx.state_dir);
    let job_id = store
        .start(JobStart {
            command: spec.command().verb(),
            profile: Some(spec.audit_target()),
            args_masked: &masked_args,
            started_at,
            pid,
        })
        .await
        .map_err(|e| e.to_string())?;

    let registry = ctx.jobs.secret_registry().clone();
    let audit = Arc::clone(&ctx.audit);
    let store_for_task = Arc::clone(&store);
    // 이 잡이 바꾼 프로파일의 상태 프로브를 낡은 것으로 만든다
    // (`crate::web::cache::invalidate_profile` doc — 과잉 무효화는 안전한 방향이다).
    let cache_ctx = Arc::clone(ctx);
    let cache_profiles = touched;

    tokio::spawn(async move {
        let completion = match running.wait_with_output().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(job_id = %job_id, error = %e, "prune 자식 대기 실패");
                return;
            }
        };
        let stdout = registry.mask(&completion.stdout);
        let stderr = registry.mask(&completion.stderr);
        if !stdout.is_empty() {
            let _ = store_for_task
                .append_log(&job_id, LogStream::Stdout, &stdout)
                .await;
        }
        if !stderr.is_empty() {
            let _ = store_for_task
                .append_log(&job_id, LogStream::Stderr, &stderr)
                .await;
        }
        if let Err(e) = store_for_task
            .finish_persistent(&job_id, completion.outcome)
            .await
        {
            tracing::warn!(job_id = %job_id, error = %e, "prune 종료 기록 실패");
        }
        for profile in &cache_profiles {
            crate::web::cache::invalidate_profile(&cache_ctx, profile);
        }

        if let Err(e) = audit
            .record(AuditEvent {
                actor: AUDIT_ACTOR,
                action: audit_action,
                target: &audit_target,
                args_masked: &masked_args,
                outcome: completion.outcome.audit_outcome(),
                exit_code: completion.outcome.exit_code(),
            })
            .await
        {
            tracing::warn!(job_id = %job_id, error = %e, "prune 완료 감사 기록 실패");
        }
    });

    Ok(job_id)
}

/// config에서 프로파일 이름 목록을 읽는다(`routes::backup`과 같은 얕은 경로).
fn load_profile_names(ctx: &ServeConfig) -> Vec<String> {
    let Some(path) = ctx.config_path.as_ref() else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::file::Config::from_toml_str(&text) else {
        return Vec::new();
    };
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    names.sort();
    names
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::i18n::Lang;
    use crate::web::auth::{self, AuthState};
    use crate::web::job::{JobRunner, JobSecrets};
    use axum::body::Body;
    use axum::http::Request;
    use axum::middleware;
    use axum::routing::{get, post};
    use axum::{Extension, Router};
    use http_body_util::BodyExt;
    use std::path::{Path as FsPath, PathBuf};
    use tower::ServiceExt;

    const TEST_TOKEN: &str = "test-token-prune-8a3f21";
    const PROFILE: &str = "prod";

    /// 이 모듈의 라우트 테스트가 공유하는 인증 상태와 그 상태에서 유효한 세션 ID.
    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    fn session() -> &'static str {
        &auth_and_session().1
    }

    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(PRUNE_PATH, get(form))
            .route(PRUNE_PLAN_PATH, post(plan))
            .route(PRUNE_APPLY_PATH, post(apply))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .layer(Extension(Arc::clone(&auth_and_session().0)))
            .with_state(ctx)
    }

    /// 가짜 `x-backup` — `--dry-run`이면 심어 둔 계획을, 아니면 실행 결과를 낸다.
    ///
    /// 실행(force) 호출은 `applied` 파일에 줄을 남긴다. "확인하면 **실제로** 삭제가
    /// 실행된다"를 그 줄의 존재로 단정할 수 있게 하려는 것이다 — 응답 코드만 보면
    /// 자식을 띄우지 않고도 통과하는 테스트가 된다.
    #[cfg(unix)]
    fn fake_exe(dir: &FsPath) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-x-backup");
        let script = r#"#!/bin/sh
dir="$(dirname "$0")"
dry=0
for a in "$@"; do
  case "$a" in --dry-run) dry=1 ;; esac
done
if [ "$dry" = "1" ]; then
  echo "dry" >> "$dir/spawns"
  cat "$dir/plan.json"
else
  echo "force" >> "$dir/spawns"
  echo "force $*" >> "$dir/applied"
  cat "$dir/plan.json"
fi
exit 0
"#;
        std::fs::write(&path, script).expect("가짜 실행 파일 쓰기 실패");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
            .expect("실행 권한 부여 실패");
        path
    }

    #[cfg(unix)]
    fn ctx_with_fake_exe(tag: &str) -> (Arc<ServeConfig>, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "x-backup-prune-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("스크래치 생성 실패");
        let exe = fake_exe(&dir);
        let config = dir.join("x-backup.toml");
        std::fs::write(
            &config,
            format!(
                "[profiles.{PROFILE}.source]\nuri_env = \"XB_PRUNE_TEST_URI\"\n\n\
                 [profiles.{PROFILE}.destination]\ntype = \"local\"\npath = \"/tmp/{PROFILE}\"\n"
            ),
        )
        .expect("config 쓰기 실패");

        let mut ctx = ServeConfig::for_test();
        ctx.config_path = Some(config);
        ctx.jobs = Arc::new(JobRunner::with_exe(
            exe,
            ctx.config_path.clone(),
            Lang::En,
            JobSecrets::new(),
        ));
        (Arc::new(ctx), dir)
    }

    /// 삭제 대상 하나짜리 계획을 심는다.
    fn plant_plan(dir: &FsPath, member_ids: &[&str]) {
        let members: Vec<String> = member_ids.iter().map(|m| format!("\"{m}\"")).collect();
        let json = format!(
            r#"{{"schema":1,"profile":"{PROFILE}","store":"local:/srv/b","dry_run":true,
"policy":{{}},"targets":[{{"kind":"chain","base_id":"base-1","member_ids":[{}],
"reason":"keep-full 초과","force_only":false}}],"retained_base_ids":["base-2"],"outcome":null}}"#,
            members.join(",")
        );
        std::fs::write(dir.join("plan.json"), json).expect("계획 표본 쓰기 실패");
    }

    fn count_lines(path: &FsPath) -> usize {
        std::fs::read_to_string(path)
            .map(|s| s.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0)
    }

    async fn post_req(
        ctx: Arc<ServeConfig>,
        uri: &str,
        body: &str,
        origin: bool,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(
                header::COOKIE,
                format!("{}={}", auth::SESSION_COOKIE_NAME, session()),
            );
        if origin {
            builder = builder.header(header::ORIGIN, "http://127.0.0.1:8787");
            builder = builder.header(header::HOST, "127.0.0.1:8787");
        }
        let response = router(ctx)
            .oneshot(builder.body(Body::from(body.to_string())).unwrap())
            .await
            .expect("라우터 호출 실패");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 확인 폼에 실린 hidden 값을 뽑는다.
    fn hidden(markup: &str, field: &str) -> String {
        let needle = format!(r#"name="{field}" value=""#);
        let start = markup
            .find(&needle)
            .unwrap_or_else(|| panic!("{field} hidden 필드가 없다: {markup}"))
            + needle.len();
        let rest = &markup[start..];
        rest[..rest.find('"').expect("닫는 따옴표 없음")].to_string()
    }

    // ---- 순수 함수 ----

    #[test]
    fn fingerprint_ignores_member_order_and_retained_ids() {
        let a = view::PlanTarget {
            kind: "chain".to_string(),
            base_id: "b1".to_string(),
            member_ids: vec!["x".to_string(), "y".to_string()],
            reason: "이유".to_string(),
            force_only: false,
        };
        let mut b = a.clone();
        b.member_ids.reverse();
        assert_eq!(
            plan_fingerprint(std::slice::from_ref(&a)),
            plan_fingerprint(&[b]),
            "구성원 순서가 지문을 바꾸면 안 된다"
        );

        // 사유 문구만 달라져도 같은 집합이면 같은 지문 — 지워질 목록이 기준이다.
        let mut c = a.clone();
        c.reason = "다른 문구".to_string();
        assert_eq!(
            plan_fingerprint(std::slice::from_ref(&a)),
            plan_fingerprint(&[c])
        );
    }

    #[test]
    fn fingerprint_changes_when_the_deletion_set_changes() {
        let base = view::PlanTarget {
            kind: "chain".to_string(),
            base_id: "b1".to_string(),
            member_ids: vec!["x".to_string()],
            reason: "이유".to_string(),
            force_only: false,
        };
        let one = plan_fingerprint(std::slice::from_ref(&base));

        let mut added = base.clone();
        added.member_ids.push("y".to_string());
        assert_ne!(
            one,
            plan_fingerprint(&[added]),
            "구성원이 늘었는데 지문이 같다"
        );

        let mut leftover = base.clone();
        leftover.force_only = true;
        assert_ne!(
            one,
            plan_fingerprint(&[leftover]),
            "잔재 여부가 지문에 없다"
        );

        assert_ne!(one, plan_fingerprint(&[]), "빈 계획이 같은 지문을 낸다");
    }

    #[test]
    fn retention_overrides_reject_non_numbers_instead_of_silently_ignoring() {
        let ok = FormBody::parse("keep_full=3&keep_days=&min_redundancy=1");
        let parsed = RetentionOverrides::from_form(&ok).expect("정상 값이 거부됐다");
        assert_eq!(parsed.keep_full, Some(3));
        assert_eq!(parsed.keep_days, None, "빈 값은 미지정이다");
        assert_eq!(parsed.min_redundancy, Some(1));

        let bad = FormBody::parse("keep_full=three");
        let err = RetentionOverrides::from_form(&bad).expect_err("오타가 조용히 무시됐다");
        assert!(
            err.contains("keep-full"),
            "어느 필드인지 말해야 한다: {err}"
        );
    }

    #[test]
    fn parse_plan_rejects_broken_and_foreign_schemas() {
        assert_eq!(parse_plan("").unwrap_err(), PlanError::Empty);
        assert!(matches!(
            parse_plan("not json").unwrap_err(),
            PlanError::Malformed(_)
        ));
        assert_eq!(
            parse_plan(r#"{"schema":99,"targets":[]}"#).unwrap_err(),
            PlanError::SchemaMismatch {
                found: 99,
                expected: EXPECTED_PRUNE_SCHEMA
            }
        );
        let bomb = "[".repeat(50_000);
        assert!(matches!(
            parse_plan(&bomb).unwrap_err(),
            PlanError::TooDeep { .. }
        ));
    }

    // ---- 라우트 ----

    /// **확인 화면을 거치지 않은 직접 POST는 403이고 자식을 띄우지 않는다.**
    #[cfg(unix)]
    #[tokio::test]
    async fn a_direct_post_without_a_confirm_token_is_refused_without_spawning() {
        let (ctx, dir) = ctx_with_fake_exe("direct");
        plant_plan(&dir, &["base-1"]);

        let (status, _) = post_req(
            Arc::clone(&ctx),
            PRUNE_APPLY_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}=whatever"),
            true,
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            count_lines(&dir.join("spawns")),
            0,
            "토큰 없는 요청이 자식을 띄웠다"
        );
        assert_eq!(count_lines(&dir.join("applied")), 0, "삭제가 실행됐다");
    }

    /// 출처가 다른 요청도 자식 없이 끊긴다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_foreign_origin_is_refused_without_spawning() {
        let (ctx, dir) = ctx_with_fake_exe("origin");
        plant_plan(&dir, &["base-1"]);

        let (status, _) = post_req(
            Arc::clone(&ctx),
            PRUNE_APPLY_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            false,
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(count_lines(&dir.join("spawns")), 0);
    }

    /// **E2E**: 계획을 보고 → 이름을 정확히 타이핑해 승인 → 실제로 삭제가 실행된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn previewing_then_confirming_actually_runs_the_deletion() {
        let (ctx, dir) = ctx_with_fake_exe("e2e");
        plant_plan(&dir, &["base-1", "incr-1"]);

        // 1) 계획 — 삭제 대상이 화면에 뜬다.
        let (status, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_KEEP_FULL}=2"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{markup}");
        assert!(markup.contains("base-1"), "삭제 대상이 화면에 없다");
        assert!(markup.contains("incr-1"));
        assert_eq!(count_lines(&dir.join("applied")), 0, "계획 단계가 삭제했다");

        // 확인 폼이 실어 보낸 값들.
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        assert!(!token.is_empty() && !fingerprint.is_empty());
        assert!(
            markup.contains(&format!(r#"name="{FIELD_KEEP_FULL}" value="2""#)),
            "보존 기준이 확인 폼에 되실리지 않았다"
        );

        // 2) 승인 — 이름을 정확히 타이핑.
        let body = format!(
            "{}={token}&{}={PROFILE}&{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}={fingerprint}&{FIELD_KEEP_FULL}=2",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;
        assert_eq!(
            status,
            StatusCode::SEE_OTHER,
            "승인이 실행으로 이어지지 않았다"
        );

        // 3) 실제로 삭제 자식이 떴다.
        for _ in 0..50 {
            if count_lines(&dir.join("applied")) > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let applied = std::fs::read_to_string(dir.join("applied")).unwrap_or_default();
        assert!(!applied.is_empty(), "확인했는데 삭제가 실행되지 않았다");
        assert!(
            applied.contains("--force"),
            "실행 argv에 --force가 없다: {applied}"
        );
        assert!(
            applied.contains("--keep-full 2"),
            "보존 기준이 실행 argv에 반영되지 않았다: {applied}"
        );
    }

    /// **계획이 바뀌면 재확인을 요구한다** — 승인과 실행 사이에 저장소가 달라진 경우.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_changed_plan_forces_re_confirmation_and_deletes_nothing() {
        let (ctx, dir) = ctx_with_fake_exe("toctou");
        plant_plan(&dir, &["base-1"]);

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            true,
        )
        .await;
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);

        // 승인 직전에 저장소가 바뀐다 — 지워질 목록이 늘었다.
        plant_plan(&dir, &["base-1", "base-3"]);

        let body = format!(
            "{}={token}&{}={PROFILE}&{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, page) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;

        assert_eq!(status, StatusCode::OK, "재확인 화면이 아니다");
        assert!(
            page.contains("plan changed") || page.contains("계획이 바뀌"),
            "계획이 바뀌었다는 안내가 없다: {page}"
        );
        assert_eq!(
            count_lines(&dir.join("applied")),
            0,
            "계획이 바뀌었는데 삭제가 실행됐다"
        );
        // 새 계획이 화면에 실려 있어야 다시 승인할 수 있다.
        assert!(page.contains("base-3"), "새 계획이 표시되지 않았다");
    }

    /// **프로파일명을 틀리게 타이핑하면 403이고 아무것도 지우지 않는다.**
    #[cfg(unix)]
    #[tokio::test]
    async fn a_mistyped_profile_name_is_refused_and_deletes_nothing() {
        let (ctx, dir) = ctx_with_fake_exe("typo");
        plant_plan(&dir, &["base-1"]);

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            true,
        )
        .await;
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);

        let body = format!(
            "{}={token}&{}=prodd&{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(
            count_lines(&dir.join("applied")),
            0,
            "이름을 틀렸는데 지워졌다"
        );
    }

    /// 같은 확인 토큰을 두 번 쓸 수 없다(가드의 재생 방지가 이 화면에서도 산다).
    #[cfg(unix)]
    #[tokio::test]
    async fn the_same_confirmation_cannot_be_replayed() {
        let (ctx, dir) = ctx_with_fake_exe("replay");
        plant_plan(&dir, &["base-1"]);

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            true,
        )
        .await;
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        let body = format!(
            "{}={token}&{}={PROFILE}&{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );

        let (first, _) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;
        assert_eq!(first, StatusCode::SEE_OTHER);

        let (second, _) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;
        assert_eq!(second, StatusCode::FORBIDDEN, "같은 확인이 두 번 통과했다");
    }

    /// **쓰기 완료가 상태 캐시를 무효화한다** — t19의 배선 단정.
    ///
    /// 캐시 **함수**가 도는지는 `crate::web::cache`의 단위 테스트가 이미 본다. 여기서 보는
    /// 것은 **배선**이다: 잡이 끝난 자리에서 그 함수가 실제로 불리는가. 둘은 다른 질문이고,
    /// 배선이 빠지면 캐시는 완벽히 동작하면서 화면은 계속 낡은 값을 보여준다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_finished_prune_invalidates_that_profiles_probe() {
        use crate::web::cache::{self, ProbeKey, ProbeOutcome, ProbeOutput};

        let (ctx, dir) = ctx_with_fake_exe("cacheinv");
        plant_plan(&dir, &["base-1"]);

        // 이 컨텍스트의 프로파일 프로브를 캐시에 하나 심는다. 캐시를 채우는 공개 경로는
        // `fetch` 하나라 그것을 쓴다(자식은 뜨지 않는다 — 클로저가 값을 그대로 준다).
        let key = ProbeKey::Profile {
            config: cache::config_key(&ctx),
            profile: PROFILE.to_string(),
        };
        cache::probe_cache()
            .fetch(key.clone(), || async {
                Arc::new(ProbeOutput {
                    outcome: ProbeOutcome::Completed {
                        outcome: crate::web::job::JobOutcome::Succeeded,
                        stdout: String::new(),
                        stderr: String::new(),
                    },
                    elapsed: std::time::Duration::from_millis(1),
                })
            })
            .await;
        assert!(
            cache::probe_cache().peek(&key).is_some(),
            "테스트 전제: 캐시에 값이 있다"
        );

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            true,
        )
        .await;
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        let body = format!(
            "{}={token}&{}={PROFILE}&{FIELD_PROFILE}={PROFILE}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), PRUNE_APPLY_PATH, &body, true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        let mut cleared = false;
        for _ in 0..100 {
            if cache::probe_cache().peek(&key).is_none() {
                cleared = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            cleared,
            "prune이 끝났는데 그 프로파일의 상태 캐시가 남아 있다 — 대시보드가 방금 지운 \
             백업을 계속 보여준다"
        );
    }

    /// 지울 것이 없으면 확인 토큰을 발급하지 않는다 — 누를 수 있는 파괴적 버튼을 만들지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_empty_plan_issues_no_confirmation() {
        let (ctx, dir) = ctx_with_fake_exe("empty");
        std::fs::write(
            dir.join("plan.json"),
            r#"{"schema":1,"profile":"prod","store":"s","dry_run":true,"policy":{},
"targets":[],"retained_base_ids":["base-1"],"outcome":null}"#,
        )
        .unwrap();

        let (status, markup) = post_req(
            Arc::clone(&ctx),
            PRUNE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            !markup.contains(guard::FIELD_CONFIRM_TOKEN),
            "빈 계획에 확인 토큰이 발급됐다"
        );
    }
}
