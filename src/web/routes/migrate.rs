//! `GET /migrate` · `POST /migrate/plan` · `POST /migrate/apply` — 프로파일 간 직접 복사.
//!
//! ## `prune`(t23)과 같은 골격, 다른 위험
//! 세 걸음(폼 → 계획 → 실행)·확인 토큰·계획 지문은 [`crate::web::routes::prune`]이 확립한
//! 그대로다(그 헤더가 각 판단의 근거를 담고 있다). 다른 것은 **무엇을 확인시키는가**다:
//!
//! - `prune`은 "무엇이 사라지는가"(삭제 대상 체인)를 보여준다.
//! - `migrate`는 "무엇을 덮어쓰는가"(target의 충돌 네임스페이스)와 **어디에 연결되는가**
//!   (양쪽 토폴로지·버전)를 보여준다.
//!
//! 후자가 이 화면에 필요한 이유: `migrate`의 대상은 **다른 살아 있는 서버**다. 백업 저장소를
//! 지우는 것과 달리, 잘못된 target을 고르면 프로덕션 DB에 남의 데이터가 섞여 들어간다.
//! 그래서 계획은 "이 작업이 어느 서버에 연결되는가"를 먼저 말한다.
//!
//! ## 이름을 다시 타이핑하게 하는 대상은 **target**이다
//! [`crate::web::guard`]의 확인은 프로파일명 재입력을 요구하는데, 이 화면은 그 이름으로
//! **target 프로파일**을 쓴다. 파괴적 효과가 일어나는 쪽이 target이기 때문이다 — source는
//! 읽기만 한다. `prune`에서는 둘이 같아서 드러나지 않던 구분이다.
//!
//! ## `--drop`과 확인 토글
//! CLI의 `--drop`은 "target의 기존 컬렉션을 복원 전에 지운다"이고 기본은 꺼짐이다. 웹에서는
//! 그것을 [`crate::web::guard::OverwriteOption`] 체크박스로 노출한다 — **충돌이 있을 때만**
//! 띄운다. 충돌이 없으면 지울 것도 없어서 그 토글이 아무 의미가 없고, 의미 없는 위험한
//! 토글을 화면에 두면 사람이 습관적으로 켜게 된다.
//!
//! `--force`(가드레일 해제)는 노출하지 않는다. 그 플래그는 "target에 데이터가 있어도
//! 진행하라"는 뜻인데, 웹에서는 그 판단을 **확인 화면 자체가** 이미 받았다 — 충돌 목록을
//! 보여주고 target 이름을 타이핑하게 한 것이 그 승인이다. 같은 승인을 체크박스로 한 번 더
//! 받으면 무엇에 동의하는지 흐려진다.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use maud::Markup;
use sha2::{Digest, Sha256};

use crate::i18n::Lang;
use crate::web::audit::{AuditEvent, AuditReceipt};
use crate::web::guard::{
    self, ConfirmError, ConfirmSubmission, DestructiveContext, DestructiveRequest,
    DestructiveTarget,
};
use crate::web::job::args::ProfileName;
use crate::web::job::{JobCommand, JobFlag, JobSpec};
use crate::web::jsonguard;
use crate::web::routes::form::FormBody;
use crate::web::state::jobs::{self, JobId, JobStart, JobStore, LogStream};
use crate::web::view::{layout, migrate as view};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수
// ---------------------------------------------------------------------------

/// 폼 화면.
pub const MIGRATE_PATH: &str = "/migrate";
/// dry-run 계획 요청(POST).
pub const MIGRATE_PLAN_PATH: &str = "/migrate/plan";
/// 실제 실행 요청(POST).
pub const MIGRATE_APPLY_PATH: &str = "/migrate/apply";
/// 결과 화면의 라우터 패턴.
pub const MIGRATE_RESULT_ROUTE: &str = "/migrate/job/{job_id}";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const MIGRATE_TITLE: &str = "Migrate";

/// 감사 로그 `actor` 값.
const AUDIT_ACTOR: &str = "web";

/// source 프로파일 필드.
pub const FIELD_PROFILE: &str = "profile";
/// target 프로파일 필드.
pub const FIELD_TARGET_PROFILE: &str = "target_profile";
/// 선택적 db 필드.
pub const FIELD_DB: &str = "db";
/// 선택적 collection 필드.
pub const FIELD_COLLECTION: &str = "collection";
/// 승인 요청이 되돌려주는 계획 지문 필드.
pub const FIELD_FINGERPRINT: &str = "plan_fingerprint";

/// `migrate --json`이 낼 스키마 버전의 웹 쪽 사본(`routes::prune`과 같은 사정).
const EXPECTED_MIGRATE_SCHEMA: u32 = 1;

/// 자식 `migrate --dry-run` 실행 상한.
///
/// dry-run은 양쪽 서버에 연결해 버전과 네임스페이스 통계를 묻는다 — 네트워크 왕복 두 번에
/// 카운트 질의가 붙으므로 `peek`(30초)보다 넉넉히 잡는다. 실제 전송은 이 상한을 쓰지
/// 않는다(백그라운드 잡이고 몇 시간이 걸릴 수 있다).
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 결과 화면 링크.
pub fn migrate_result_href(id: &JobId) -> String {
    format!("/migrate/job/{id}")
}

// ---------------------------------------------------------------------------
// 요청 값
// ---------------------------------------------------------------------------

/// 폼에서 읽은 마이그레이션 요청.
#[derive(Debug, Clone)]
struct MigrateRequest {
    source: ProfileName,
    target: ProfileName,
    db: Option<String>,
    collection: Option<String>,
}

impl MigrateRequest {
    /// 폼에서 읽고 검증한다.
    ///
    /// `--collection`은 `--db` 없이 쓸 수 없다 — CLI가 `requires = "db"`로 강제하는 규칙을
    /// **여기서 먼저** 막는다. 자식에게 맡기면 exit 2(사용법 오류)로 돌아오는데, 그건
    /// "웹이 만든 argv가 잘못됐다"는 뜻이라 운영자가 고칠 수 있는 것이 없다. 폼 단계에서
    /// 막으면 사람이 고칠 수 있는 문장이 된다.
    ///
    /// source와 target이 같으면 거부한다. CLI는 그것을 막지 않지만(자기 자신으로 복사하는
    /// 것이 문법적으로 오류는 아니다), 웹에서 두 드롭다운을 같은 값으로 두는 것은 거의 항상
    /// 실수다 — 그리고 그 실수의 결과가 프로덕션 위에 자기 자신을 덮어쓰는 것이다.
    fn from_form(form: &FormBody, lang: Lang) -> std::result::Result<Self, String> {
        let source = ProfileName::parse(form.get(FIELD_PROFILE).unwrap_or_default(), lang)
            .map_err(|e| format!("source profile: {e}"))?;
        let target = ProfileName::parse(form.get(FIELD_TARGET_PROFILE).unwrap_or_default(), lang)
            .map_err(|e| format!("target profile: {e}"))?;

        if source.as_str() == target.as_str() {
            return Err(format!(
                "source와 target이 같은 프로파일('{}')입니다 — 자기 자신 위에 복사합니다.",
                source.as_str()
            ));
        }

        let db = form.non_empty(FIELD_DB).map(str::to_string);
        let collection = form.non_empty(FIELD_COLLECTION).map(str::to_string);
        if collection.is_some() && db.is_none() {
            return Err(
                "collection만 지정할 수 없습니다 — db를 함께 지정하세요(컬렉션 이름만 주면 \
                 모든 데이터베이스의 동명 컬렉션이 대상이 됩니다)."
                    .to_string(),
            );
        }

        Ok(Self {
            source,
            target,
            db,
            collection,
        })
    }

    /// 확인 폼에 되실을 값들.
    fn as_hidden(&self, fingerprint: &str) -> Vec<(String, String)> {
        let mut fields = vec![
            (FIELD_PROFILE.to_string(), self.source.as_str().to_string()),
            (
                FIELD_TARGET_PROFILE.to_string(),
                self.target.as_str().to_string(),
            ),
            (FIELD_FINGERPRINT.to_string(), fingerprint.to_string()),
        ];
        if let Some(db) = &self.db {
            fields.push((FIELD_DB.to_string(), db.clone()));
        }
        if let Some(collection) = &self.collection {
            fields.push((FIELD_COLLECTION.to_string(), collection.clone()));
        }
        fields
    }

    /// 이 잡이 상태를 바꾸는 프로파일들 — 캐시 무효화 대상.
    ///
    /// **source도 포함한다.** 데이터를 읽기만 하니 안 바뀔 것 같지만, 대규모 읽기는 그
    /// 서버의 부하·연결 수를 바꾸고 대시보드가 보는 것이 바로 그 값이다. 과잉 무효화의
    /// 비용은 다음 화면 로드에서 프로브 한 번뿐이다(`crate::web::cache` 헤더).
    fn touched_profiles(&self) -> Vec<String> {
        vec![
            self.source.as_str().to_string(),
            self.target.as_str().to_string(),
        ]
    }

    /// `JobSpec` 뼈대(공통 부분).
    fn base_spec(&self, lang: Lang) -> std::result::Result<JobSpec, String> {
        let mut spec = JobSpec::new(JobCommand::Migrate, lang)
            .with_profile(self.source.clone())
            .with_target_profile(self.target.clone());
        if let Some(db) = &self.db {
            spec = spec.with_db(db).map_err(|e| e.detail())?;
        }
        if let Some(collection) = &self.collection {
            spec = spec.with_collection(collection).map_err(|e| e.detail())?;
        }
        Ok(spec)
    }
}

// ---------------------------------------------------------------------------
// 자식 stdout → 계획
// ---------------------------------------------------------------------------

/// 계획을 못 읽은 이유.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanError {
    /// stdout이 비었다.
    Empty,
    /// JSON이 아니거나 기대한 모양이 아니다.
    Malformed(String),
    /// 스키마 버전이 다르다.
    SchemaMismatch {
        /// 자식이 낸 값.
        found: u64,
        /// 이 빌드가 아는 값.
        expected: u32,
    },
    /// 중첩이 너무 깊어 파싱하지 않았다.
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
                    "migrate produced no plan. Read the exit code — the child usually stopped before it could connect.",
                    "migrate가 계획을 내지 않았습니다. 종료 코드를 보세요 — 대개 접속 전에 자식이 멈춘 경우입니다.",
                )
                .to_string(),
            PlanError::Malformed(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "migrate output could not be parsed as the expected JSON:",
                    "migrate 출력을 기대한 JSON으로 해석할 수 없습니다:",
                )
            ),
            PlanError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} \u{2260} {expected})",
                lang.sel(
                    "migrate reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "migrate가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            PlanError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "migrate output is nested more deeply than this console parses.",
                    "migrate 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다.",
                )
            ),
        }
    }
}

/// 자식 stdout을 계획으로 접는다 — **순수 함수**.
pub fn parse_plan(
    stdout: &str,
    source: &str,
    target: &str,
) -> std::result::Result<view::Plan, PlanError> {
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
        Some(found) if found == u64::from(EXPECTED_MIGRATE_SCHEMA) => {}
        Some(found) => {
            return Err(PlanError::SchemaMismatch {
                found,
                expected: EXPECTED_MIGRATE_SCHEMA,
            })
        }
        None => {
            return Err(PlanError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }

    // `dry_run: true`가 아니면 이것은 계획이 아니라 완료 요약이다 — 그 둘을 섞으면
    // "옮기지 않았다"고 그린 화면이 실제로는 옮긴 뒤의 문서를 보여주게 된다.
    if value.get("dry_run").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(PlanError::Malformed(
            "dry-run 계획이 아닙니다(`dry_run`이 true가 아님)".to_string(),
        ));
    }

    let namespaces: Vec<view::NamespaceRow> = value
        .get("namespaces")
        .and_then(serde_json::Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|item| view::NamespaceRow {
                    ns: string_at(item, "ns"),
                    source: u64_at(item, "source"),
                    target: u64_at(item, "target"),
                })
                .collect()
        })
        .unwrap_or_default();

    let conflicting: Vec<String> = value
        .get("conflicting_namespaces")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();

    let fingerprint = plan_fingerprint(&namespaces, &conflicting);

    Ok(view::Plan {
        profile: source.to_string(),
        target_profile: target.to_string(),
        source_version: string_at(&value, "source_server_version"),
        source_topology: string_at(&value, "source_topology"),
        target_version: string_at(&value, "target_server_version"),
        scope: value
            .get("ns")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        conflicting,
        version_warning: value
            .get("version_warning")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        namespaces,
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

fn u64_at(value: &serde_json::Value, key: &str) -> u64 {
    value
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

/// 계획의 지문.
///
/// **네임스페이스 집합과 충돌 목록**을 반영한다 — 이 화면에서 운영자가 승인하는 것은
/// "이 네임스페이스들을 옮기고, 이것들을 덮어쓴다"이기 때문이다.
///
/// 문서 수(추정치)는 **일부러 넣지 않는다.** 살아 있는 서버의 문서 수는 초 단위로 변한다 —
/// 그것을 지문에 넣으면 정상 트래픽만으로 승인이 계속 무효가 되고, 그러면 이 안전장치는
/// 곧 "무조건 다시 눌러야 하는 버튼"이 되어 의미를 잃는다(`routes::prune` 헤더의 같은
/// 판단: 자주 틀리는 경고는 사람이 무시한다). 반면 네임스페이스가 **생기거나 사라지는**
/// 것은 승인의 전제가 달라진 것이므로 잡는다.
pub fn plan_fingerprint(namespaces: &[view::NamespaceRow], conflicting: &[String]) -> String {
    let mut names: Vec<&str> = namespaces.iter().map(|n| n.ns.as_str()).collect();
    names.sort_unstable();
    let mut conflicts: Vec<&str> = conflicting.iter().map(String::as_str).collect();
    conflicts.sort_unstable();

    let mut hasher = Sha256::new();
    for name in names {
        hasher.update(name.as_bytes());
        hasher.update([0x1f]);
    }
    hasher.update([0x1d]);
    for name in conflicts {
        hasher.update(name.as_bytes());
        hasher.update([0x1f]);
    }
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /migrate` — 폼.
pub async fn form(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let profiles = load_profile_names(&ctx);
    layout::shell(
        ctx.lang,
        MIGRATE_TITLE,
        view::form_body(ctx.lang, &profiles, None, None, None),
    )
}

/// `POST /migrate/plan` — dry-run으로 계획을 받아 확인 화면과 함께 보여준다.
pub async fn plan(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    let request = match MigrateRequest::from_form(&form_body, ctx.lang) {
        Ok(r) => r,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

    match run_dry_run(&ctx, &request).await {
        Ok(plan) => render_plan(&ctx, &request, plan, None).await,
        Err(message) => form_with_notice(&ctx, StatusCode::BAD_GATEWAY, &form_body, &message),
    }
}

/// `POST /migrate/apply` — 확인을 검증하고 실제로 옮긴다.
///
/// 검사 순서는 [`crate::web::routes::prune::apply`]와 같다(그 헤더 "검사 순서").
pub async fn apply(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    // 확인 화면을 거치지 않은 직접 POST는 자식을 띄우기 전에 끊는다.
    if form_body
        .get(guard::FIELD_CONFIRM_TOKEN)
        .is_none_or(str::is_empty)
    {
        return refused(
            &ctx,
            ctx.lang.sel(
                "This request did not come from a confirmation screen. Preview the migration first, then approve it there.",
                "이 요청은 확인 화면에서 온 것이 아닙니다. 먼저 계획을 미리 보고 그 화면에서 승인하세요.",
            ),
        );
    }

    let request = match MigrateRequest::from_form(&form_body, ctx.lang) {
        Ok(r) => r,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

    // 지문 — 승인된 계획과 지금 실행될 계획이 같은가.
    let submitted = form_body.get(FIELD_FINGERPRINT).unwrap_or_default();
    let current = match run_dry_run(&ctx, &request).await {
        Ok(plan) => plan,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_GATEWAY, &form_body, &message)
        }
    };
    if current.fingerprint != submitted {
        return render_plan(
            &ctx,
            &request,
            current,
            Some(view::plan_changed_notice(ctx.lang)),
        )
        .await;
    }

    // 가드 — 이름은 **target** 프로파일이다(모듈 헤더).
    let allow_drop = form_body.checked(guard::FIELD_ALLOW_OVERWRITE);
    let submission = ConfirmSubmission {
        token: form_body.get(guard::FIELD_CONFIRM_TOKEN),
        typed_name: form_body.get(guard::FIELD_CONFIRM_NAME).unwrap_or_default(),
        allow_overwrite: allow_drop,
    };
    let what = DestructiveTarget {
        action: JobCommand::Migrate.audit_action(),
        target: &request.target,
    };
    let secrets = ctx.jobs.secret_registry().clone();
    let spec_for_args = match build_spec(&request, allow_drop, ctx.lang) {
        Ok(spec) => spec,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

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
                ConfirmError::ForeignOrigin(_) | ConfirmError::Rejected(_) => StatusCode::FORBIDDEN,
                ConfirmError::Audit(_) => StatusCode::INTERNAL_SERVER_ERROR,
            };
            let notice = view::guard_rejection_notice(ctx.lang, &e.explain(ctx.lang));
            let mut response = render_plan(&ctx, &request, current, Some(notice)).await;
            *response.status_mut() = status;
            return response;
        }
    };

    // 실행.
    let spec = match build_spec(&request, gate.allow_overwrite, ctx.lang) {
        Ok(spec) => spec,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };
    match start_migrate_job(&ctx, spec, gate.receipt, request.touched_profiles()).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, migrate_result_href(&id))],
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

/// `GET /migrate/job/{job_id}` — 결과. 잡 이력 화면으로 넘긴다(`routes::prune`과 같다).
pub async fn result(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, MIGRATE_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    if detail.summary.is_none() && !detail.log_file_present {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, MIGRATE_TITLE, view::unknown_job(ctx.lang)),
        )
            .into_response();
    }
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

fn refused(ctx: &Arc<ServeConfig>, message: &str) -> Response {
    (
        StatusCode::FORBIDDEN,
        layout::shell(
            ctx.lang,
            MIGRATE_TITLE,
            view::guard_rejection_notice(ctx.lang, message),
        ),
    )
        .into_response()
}

fn reject_foreign_origin(ctx: &Arc<ServeConfig>, headers: &HeaderMap) -> Option<Response> {
    crate::web::auth::verify_same_origin(headers)
        .err()
        .map(|p| refused(ctx, &p.explain(ctx.lang)))
}

fn form_with_notice(
    ctx: &Arc<ServeConfig>,
    status: StatusCode,
    form_body: &FormBody,
    message: &str,
) -> Response {
    let profiles = load_profile_names(ctx);
    let source = form_body.get(FIELD_PROFILE).map(str::to_string);
    let target = form_body.get(FIELD_TARGET_PROFILE).map(str::to_string);
    (
        status,
        layout::shell(
            ctx.lang,
            MIGRATE_TITLE,
            view::form_body(
                ctx.lang,
                &profiles,
                source.as_deref(),
                target.as_deref(),
                Some(view::validation_notice(ctx.lang, message)),
            ),
        ),
    )
        .into_response()
}

async fn render_plan(
    ctx: &Arc<ServeConfig>,
    request: &MigrateRequest,
    plan: view::Plan,
    notice: Option<Markup>,
) -> Response {
    let confirm = Some(
        ctx.guard
            .render_confirm(ctx.lang, confirm_request(ctx.lang, request, &plan))
            .await,
    );

    let mut body = view::plan_body(ctx.lang, &plan, confirm);
    if let Some(notice) = notice {
        body = maud::html! { (notice) (body) };
    }
    (StatusCode::OK, layout::shell(ctx.lang, MIGRATE_TITLE, body)).into_response()
}

fn confirm_request<'a>(
    lang: crate::i18n::Lang,
    request: &'a MigrateRequest,
    plan: &view::Plan,
) -> DestructiveRequest<'a> {
    let mut summary: Vec<(&'static str, String)> = vec![
        ("command", "migrate".to_string()),
        ("source", request.source.as_str().to_string()),
        ("target", request.target.as_str().to_string()),
        ("namespaces", plan.namespaces.len().to_string()),
        ("documents (est.)", plan.source_total().to_string()),
    ];
    if let Some(db) = &request.db {
        summary.push(("db", db.clone()));
    }
    if let Some(collection) = &request.collection {
        summary.push(("collection", collection.clone()));
    }
    if plan.has_conflicts() {
        summary.push(("existing namespaces", plan.conflicting.len().to_string()));
    }

    DestructiveRequest {
        what: DestructiveTarget {
            action: JobCommand::Migrate.audit_action(),
            target: &request.target,
        },
        headline: lang
            .sel(
                "This writes into a live server",
                "이 작업은 살아 있는 서버에 씁니다",
            )
            .to_string(),
        irreversible_notice: lang.sel(
            "The target is a running database, not a backup store. Whatever this writes cannot be undone by this console — restoring the target to its previous state needs a backup you took beforehand.",
            "target은 백업 저장소가 아니라 돌고 있는 데이터베이스입니다. 여기서 쓴 내용은 이 콘솔로 되돌릴 수 없습니다 — 이전 상태로 복구하려면 미리 받아 둔 백업이 필요합니다.",
        ).to_string(),
        summary,
        // 충돌이 있을 때만 drop 토글을 띄운다(모듈 헤더 "`--drop`과 확인 토글").
        overwrite: plan.has_conflicts().then(|| guard::OverwriteOption {
            label: lang
                .sel(
                    "Drop the target's existing collections first (--drop)",
                    "target의 기존 컬렉션을 먼저 지운다(--drop)",
                )
                .to_string(),
            hint: lang.sel(
                "Off by default, matching the CLI. Without it the copy merges into what is already there, which can leave a mix of old and new documents.",
                "CLI와 같이 기본은 꺼짐입니다. 켜지 않으면 기존 데이터에 섞여 들어가 옛 문서와 새 문서가 뒤섞인 상태가 될 수 있습니다.",
            ).to_string(),
        }),
        extra_hidden: request.as_hidden(&plan.fingerprint),
        submit_path: MIGRATE_APPLY_PATH,
        submit_label: lang
            .sel("Copy into the target", "target으로 복사")
            .to_string(),
        cancel_href: MIGRATE_PATH,
    }
}

/// 실행용 `JobSpec`.
fn build_spec(
    request: &MigrateRequest,
    allow_drop: bool,
    lang: Lang,
) -> std::result::Result<JobSpec, String> {
    let mut spec = request.base_spec(lang)?;
    if allow_drop {
        spec = spec.with_flag(JobFlag::Drop);
    }
    // `--force`는 노출하지 않는다(모듈 헤더) — 확인 화면 자체가 그 승인이다.
    Ok(spec)
}

/// dry-run 자식을 돌려 계획을 받아온다.
async fn run_dry_run(
    ctx: &Arc<ServeConfig>,
    request: &MigrateRequest,
) -> std::result::Result<view::Plan, String> {
    let spec = request.base_spec(ctx.lang)?.with_flag(JobFlag::DryRun);

    let running = ctx.jobs.spawn_preview(&spec).map_err(|e| e.to_string())?;
    let completion = tokio::time::timeout(PLAN_TIMEOUT, running.wait_with_output())
        .await
        .map_err(|_| {
            ctx.lang
                .sel(
                    "migrate --dry-run did not finish in time — one of the servers may be unreachable.",
                    "migrate --dry-run이 제한 시간 안에 끝나지 않았습니다 — 두 서버 중 한쪽에 닿지 못했을 수 있습니다.",
                )
                .to_string()
        })?
        .map_err(|e| e.to_string())?;

    parse_plan(
        &completion.stdout,
        request.source.as_str(),
        request.target.as_str(),
    )
    .map_err(|e| e.explain(ctx.lang))
}

/// 확정된 마이그레이션 잡을 띄우고 이력에 기록한다.
async fn start_migrate_job(
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
                tracing::warn!(job_id = %job_id, error = %e, "migrate 자식 대기 실패");
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
            tracing::warn!(job_id = %job_id, error = %e, "migrate 종료 기록 실패");
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
            tracing::warn!(job_id = %job_id, error = %e, "migrate 완료 감사 기록 실패");
        }
    });

    Ok(job_id)
}

/// config에서 프로파일 이름 목록을 읽는다.
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

    const TEST_TOKEN: &str = "test-token-migrate-2c9d47";
    const SOURCE: &str = "prod";
    const TARGET: &str = "dr";

    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    fn session() -> &'static str {
        &auth_and_session().1
    }

    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(MIGRATE_PATH, get(form))
            .route(MIGRATE_PLAN_PATH, post(plan))
            .route(MIGRATE_APPLY_PATH, post(apply))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .layer(Extension(Arc::clone(&auth_and_session().0)))
            .with_state(ctx)
    }

    /// 가짜 `x-backup` — dry-run이면 계획을, 아니면 실행 argv를 `applied`에 남긴다.
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
  cat "$dir/plan.json"
else
  echo "run $*" >> "$dir/applied"
  echo '{"schema":1,"migrated":true,"source_topology":"replicaSet","target_had_data":true,"ns":null}'
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
            "x-backup-migrate-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::UNIX_EPOCH
                .elapsed()
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("스크래치 생성 실패");
        let exe = fake_exe(&dir);
        let config = dir.join("x-backup.toml");
        let mut toml = String::new();
        for profile in [SOURCE, TARGET] {
            toml.push_str(&format!(
                "[profiles.{profile}.source]\nuri_env = \"XB_MIGRATE_TEST_{profile}\"\n\n\
                 [profiles.{profile}.destination]\ntype = \"local\"\npath = \"/tmp/{profile}\"\n\n"
            ));
        }
        std::fs::write(&config, toml).expect("config 쓰기 실패");

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

    /// 계획 표본을 심는다. `conflicts`가 비어 있지 않으면 충돌이 있는 계획이다.
    fn plant_plan(dir: &FsPath, namespaces: &[(&str, u64, u64)], conflicts: &[&str]) {
        let ns_items: Vec<String> = namespaces
            .iter()
            .map(|(ns, s, t)| {
                format!(r#"{{"ns":"{ns}","source":{s},"target":{t},"transfer":{s}}}"#)
            })
            .collect();
        let conflict_items: Vec<String> = conflicts.iter().map(|c| format!("\"{c}\"")).collect();
        let json = format!(
            r#"{{"schema":1,"dry_run":true,"source_server_version":"7.0.5",
"source_topology":"replicaSet","target_server_version":"6.0.9","ns":null,
"conflicting_namespaces":[{}],"version_warning":"7.0 → 6.0은 하위 호환이 보장되지 않습니다",
"namespaces":[{}],"source_total":1,"target_total":0,"transfer_total":1}}"#,
            conflict_items.join(","),
            ns_items.join(",")
        );
        std::fs::write(dir.join("plan.json"), json).expect("계획 표본 쓰기 실패");
    }

    fn applied(dir: &FsPath) -> String {
        std::fs::read_to_string(dir.join("applied")).unwrap_or_default()
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

    fn hidden(markup: &str, field: &str) -> String {
        let needle = format!(r#"name="{field}" value=""#);
        let start = markup
            .find(&needle)
            .unwrap_or_else(|| panic!("{field} hidden 필드가 없다"))
            + needle.len();
        let rest = &markup[start..];
        rest[..rest.find('"').expect("닫는 따옴표 없음")].to_string()
    }

    /// 계획을 받아 확인 폼의 토큰·지문을 뽑는다.
    #[cfg(unix)]
    async fn preview(ctx: &Arc<ServeConfig>) -> (String, String, String) {
        let (status, markup) = post_req(
            Arc::clone(ctx),
            MIGRATE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{markup}");
        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        (token, fingerprint, markup)
    }

    // ---- 폼 검증 ----

    #[test]
    fn a_collection_without_a_db_is_refused() {
        let form = FormBody::parse(&format!(
            "{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_COLLECTION}=orders"
        ));
        let err =
            MigrateRequest::from_form(&form, Lang::En).expect_err("db 없는 collection이 통과했다");
        assert!(
            err.contains("db"),
            "무엇을 고쳐야 하는지 말해야 한다: {err}"
        );
    }

    #[test]
    fn migrating_a_profile_onto_itself_is_refused() {
        let form = FormBody::parse(&format!(
            "{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={SOURCE}"
        ));
        let err =
            MigrateRequest::from_form(&form, Lang::En).expect_err("자기 자신으로 복사가 통과했다");
        assert!(err.contains(SOURCE));
    }

    /// 완료 요약을 계획으로 오해하지 않는다 — `dry_run: true`가 아니면 거부한다.
    #[test]
    fn a_completion_summary_is_not_accepted_as_a_plan() {
        let summary = r#"{"schema":1,"migrated":true,"source_topology":"replicaSet",
"target_had_data":true,"ns":null}"#;
        assert!(matches!(
            parse_plan(summary, SOURCE, TARGET).unwrap_err(),
            PlanError::Malformed(_)
        ));
    }

    #[test]
    fn parse_plan_reads_versions_topology_and_conflicts() {
        let json = r#"{"schema":1,"dry_run":true,"source_server_version":"7.0.5",
"source_topology":"replicaSet","target_server_version":"6.0.9","ns":"shop",
"conflicting_namespaces":["shop.orders"],"version_warning":"경고",
"namespaces":[{"ns":"shop.orders","source":10,"target":3}]}"#;
        let plan = parse_plan(json, SOURCE, TARGET).expect("계획 파싱 실패");
        assert_eq!(plan.source_version, "7.0.5");
        assert_eq!(plan.source_topology, "replicaSet");
        assert_eq!(plan.target_version, "6.0.9");
        assert_eq!(plan.scope.as_deref(), Some("shop"));
        assert_eq!(plan.conflicting, vec!["shop.orders".to_string()]);
        assert_eq!(plan.version_warning.as_deref(), Some("경고"));
        assert_eq!(plan.namespaces.len(), 1);
        assert_eq!(plan.namespaces[0].target, 3);
    }

    /// **문서 수는 지문에 넣지 않는다** — 살아 있는 서버의 카운트는 초 단위로 변한다.
    #[test]
    fn document_counts_do_not_change_the_fingerprint() {
        let a = vec![view::NamespaceRow {
            ns: "shop.orders".to_string(),
            source: 10,
            target: 0,
        }];
        let b = vec![view::NamespaceRow {
            ns: "shop.orders".to_string(),
            source: 999_999,
            target: 42,
        }];
        assert_eq!(
            plan_fingerprint(&a, &[]),
            plan_fingerprint(&b, &[]),
            "문서 수가 지문을 바꾸면 정상 트래픽만으로 승인이 무효가 된다"
        );
    }

    /// 네임스페이스가 생기거나 충돌 목록이 달라지면 지문이 바뀐다.
    #[test]
    fn fingerprint_tracks_namespaces_and_conflicts() {
        let one = vec![view::NamespaceRow {
            ns: "a.b".to_string(),
            source: 1,
            target: 0,
        }];
        let base = plan_fingerprint(&one, &[]);

        let mut two = one.clone();
        two.push(view::NamespaceRow {
            ns: "a.c".to_string(),
            source: 1,
            target: 0,
        });
        assert_ne!(base, plan_fingerprint(&two, &[]), "ns가 늘었는데 같다");
        assert_ne!(
            base,
            plan_fingerprint(&one, &["a.b".to_string()]),
            "충돌이 생겼는데 같다"
        );
    }

    // ---- 라우트 ----

    /// **done_criteria**: 계획에 연결·버전·충돌 네임스페이스가 표시된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_plan_screen_shows_connection_versions_and_conflicts() {
        let (ctx, dir) = ctx_with_fake_exe("plan");
        plant_plan(&dir, &[("shop.orders", 1200, 340)], &["shop.orders"]);

        let (_, _, markup) = preview(&ctx).await;

        assert!(markup.contains("replicaSet"), "연결(토폴로지)이 없다");
        assert!(
            markup.contains("7.0.5") && markup.contains("6.0.9"),
            "버전이 없다"
        );
        assert!(markup.contains("shop.orders"), "충돌 네임스페이스가 없다");
        assert!(markup.contains("하위 호환"), "버전 경고가 없다");
        assert!(applied(&dir).is_empty(), "계획 단계가 실행했다");
    }

    /// **done_criteria**: `--drop`이 파괴적 가드를 탄다 — 확인 없이는 붙지 않고,
    /// 확인 화면의 토글을 켰을 때만 실행 argv에 실린다.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_drop_option_only_reaches_argv_through_the_guard() {
        let (ctx, dir) = ctx_with_fake_exe("drop");
        plant_plan(&dir, &[("shop.orders", 10, 3)], &["shop.orders"]);

        // 충돌이 있으므로 확인 화면에 drop 토글이 뜬다.
        let (token, fingerprint, markup) = preview(&ctx).await;
        assert!(
            markup.contains(guard::FIELD_ALLOW_OVERWRITE),
            "충돌이 있는데 drop 토글이 없다"
        );

        // 토글을 켜고 승인.
        let body = format!(
            "{}={token}&{}={TARGET}&{}=1&{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
            guard::FIELD_ALLOW_OVERWRITE,
        );
        let (status, _) = post_req(Arc::clone(&ctx), MIGRATE_APPLY_PATH, &body, true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        for _ in 0..50 {
            if !applied(&dir).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let argv = applied(&dir);
        assert!(!argv.is_empty(), "승인했는데 실행되지 않았다");
        assert!(
            argv.contains("--drop"),
            "토글을 켰는데 --drop이 없다: {argv}"
        );
        assert!(
            argv.contains("--target-profile dr"),
            "target이 argv에 없다: {argv}"
        );
    }

    /// 토글을 켜지 않으면 `--drop`이 붙지 않는다(기본은 꺼짐).
    #[cfg(unix)]
    #[tokio::test]
    async fn without_the_toggle_drop_is_not_passed() {
        let (ctx, dir) = ctx_with_fake_exe("nodrop");
        plant_plan(&dir, &[("shop.orders", 10, 3)], &["shop.orders"]);

        let (token, fingerprint, _) = preview(&ctx).await;
        let body = format!(
            "{}={token}&{}={TARGET}&{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), MIGRATE_APPLY_PATH, &body, true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        for _ in 0..50 {
            if !applied(&dir).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let argv = applied(&dir);
        assert!(!argv.is_empty(), "승인했는데 실행되지 않았다");
        assert!(
            !argv.contains("--drop"),
            "토글이 꺼졌는데 --drop이 붙었다: {argv}"
        );
    }

    /// 충돌이 없으면 drop 토글 자체를 띄우지 않는다 — 의미 없는 위험한 토글을 두지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn no_conflicts_means_no_drop_toggle() {
        let (ctx, dir) = ctx_with_fake_exe("clean");
        plant_plan(&dir, &[("shop.users", 5, 0)], &[]);

        let (_, _, markup) = preview(&ctx).await;
        assert!(
            !markup.contains(guard::FIELD_ALLOW_OVERWRITE),
            "충돌이 없는데 drop 토글이 떴다"
        );
    }

    /// 확인 없이 직접 POST하면 403이고 자식을 띄우지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_direct_post_is_refused_without_running_anything() {
        let (ctx, dir) = ctx_with_fake_exe("direct");
        plant_plan(&dir, &[("a.b", 1, 0)], &[]);

        let (status, _) = post_req(
            Arc::clone(&ctx),
            MIGRATE_APPLY_PATH,
            &format!("{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}"),
            true,
        )
        .await;

        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(applied(&dir).is_empty());
    }

    /// **이름은 target 프로파일이다** — source를 타이핑하면 거부된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn typing_the_source_name_instead_of_the_target_is_refused() {
        let (ctx, dir) = ctx_with_fake_exe("wrongname");
        plant_plan(&dir, &[("a.b", 1, 0)], &[]);

        let (token, fingerprint, _) = preview(&ctx).await;
        let body = format!(
            "{}={token}&{}={SOURCE}&{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), MIGRATE_APPLY_PATH, &body, true).await;

        assert_eq!(status, StatusCode::FORBIDDEN, "source 이름으로 통과했다");
        assert!(applied(&dir).is_empty());
    }

    /// 계획이 바뀌면 재확인을 요구하고 아무것도 옮기지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_changed_plan_forces_re_confirmation() {
        let (ctx, dir) = ctx_with_fake_exe("toctou");
        plant_plan(&dir, &[("a.b", 1, 0)], &[]);
        let (token, fingerprint, _) = preview(&ctx).await;

        // 승인 직전에 네임스페이스가 하나 생겼다.
        plant_plan(&dir, &[("a.b", 1, 0), ("a.c", 2, 0)], &[]);

        let body = format!(
            "{}={token}&{}={TARGET}&{FIELD_PROFILE}={SOURCE}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, page) = post_req(Arc::clone(&ctx), MIGRATE_APPLY_PATH, &body, true).await;

        assert_eq!(status, StatusCode::OK);
        assert!(
            page.contains("plan changed") || page.contains("계획이 바뀌"),
            "재확인 안내가 없다"
        );
        assert!(applied(&dir).is_empty(), "계획이 바뀌었는데 실행됐다");
        assert!(page.contains("a.c"), "새 계획이 표시되지 않았다");
    }
}
