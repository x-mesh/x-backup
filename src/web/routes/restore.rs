//! `GET /restore` · `POST /restore/plan` · `POST /restore/apply` — 복구와 PITR.
//!
//! 세 걸음(폼 → 계획 → 실행)·확인 토큰·계획 지문은 [`crate::web::routes::prune`]이 확립한
//! 그대로다. 이 파일이 추가로 지는 책임은 둘이다: **`--at` 사전 검증**과 **대상 표시**.
//!
//! ## `--at` — 자식을 띄우기 전에 걸러낼 수 있는 것과 없는 것
//! 잘못된 시점 입력은 네 가지로 나뉘는데, 그중 **셋은 순수 문자열 검사로 끝난다**:
//!
//! | 사례 | 예 | 누가 잡나 |
//! |---|---|---|
//! | 형식 오류 | `어제`, `2026/06/12` | 이 파일([`validate_at`]) — 자식 없음 |
//! | 타임존 없음 | `2026-06-12T13:00:00` | 이 파일 — 자식 없음 |
//! | 미래 시각 | 지금보다 뒤 | 이 파일 — 자식 없음 |
//! | oplog 윈도우 밖 | 백업이 닿지 않는 과거 | **자식**(dry-run) |
//!
//! 앞의 셋을 여기서 잡는 이유는 비용이 아니라 **메시지**다. 자식에게 넘기면 exit 2와 함께
//! 파서 오류 문자열이 돌아오는데, 그건 "무엇을 어떻게 고쳐야 하는지"를 말하지 않는다.
//! 특히 **타임존 없음**은 가장 흔한 실수인데 일반 파싱 오류로 뭉개진다 — RFC 3339는
//! 오프셋을 필수로 요구하므로 `2026-06-12T13:00:00`은 형식 오류로 떨어지지만, 사람에게
//! 필요한 문장은 "형식이 틀렸다"가 아니라 "**끝에 `Z`를 붙이세요**"다.
//!
//! 네 번째는 여기서 잡을 수 없다. oplog 윈도우는 저장소에 실제로 무엇이 있는지에 달렸고,
//! 그걸 아는 유일한 방법이 자식에게 묻는 것이다. 그래서 그 거부는 dry-run 단계에서
//! 일어난다 — **복구가 실행되기 전**이라는 성질은 그대로다(확인 화면이 아예 뜨지 않는다).
//!
//! ## 대상 표시 — 이 화면에서 가장 중요한 한 줄
//! `restore`의 기본은 **in-place**다. 아무것도 고르지 않으면 백업을 떠온 그 프로덕션
//! 서버에 쓴다. 그래서 계획 화면은 가려진 대상 URI와 그 출처를 첫 줄에 놓고, in-place면
//! 경고를 하나 더 붙인다([`crate::web::view::restore`] 헤더).
//!
//! 가려진 URI는 **자식이 준 값을 그대로** 쓴다(`destination` 필드가 이미 `redact_uri`를
//! 거쳤다). 이 파일이 다시 가리지 않는 이유: 가리는 규칙이 두 곳에 생기면 한쪽만 고쳐졌을 때
//! 조용히 샌다.
//!
//! ## 원시 `--target <URI>`는 제공하지 않는다
//! CLI에는 있지만 이 콘솔에는 없다. 근거는 [`crate::web::job::spec`]이 이미 정한 것과 같다 —
//! 그 모듈은 `with_target_profile`만 두고 원시 URI를 받는 빌더를 일부러 만들지 않았다.
//! 웹 폼에 URI 입력칸을 두면 운영자가 **자격증명이 든 문자열**을 브라우저에 타이핑하게 되고,
//! 그 값은 폼 재제출·브라우저 자동완성·프록시 로그로 번진다. config에 이름으로 등록된
//! 프로파일만 대상이 될 수 있게 하면 그 표면이 아예 없다.

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, Utc};
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
use crate::web::view::layout;
use crate::web::view::restore::{self as view, Prefill, TargetOrigin};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·상수
// ---------------------------------------------------------------------------

/// 폼 화면.
pub const RESTORE_PATH: &str = "/restore";
/// dry-run 계획 요청(POST).
pub const RESTORE_PLAN_PATH: &str = "/restore/plan";
/// 실제 복구 요청(POST).
pub const RESTORE_APPLY_PATH: &str = "/restore/apply";
/// 결과 화면의 라우터 패턴.
pub const RESTORE_RESULT_ROUTE: &str = "/restore/job/{job_id}";
/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
pub const RESTORE_TITLE: &str = "Restore";

/// 감사 로그 `actor` 값.
const AUDIT_ACTOR: &str = "web";

/// 프로파일 필드.
pub const FIELD_PROFILE: &str = "profile";
/// 백업 id 필드.
pub const FIELD_ID: &str = "id";
/// PITR 시점 필드.
pub const FIELD_AT: &str = "at";
/// 복구 대상 프로파일 필드(비우면 in-place).
pub const FIELD_TARGET_PROFILE: &str = "target_profile";
/// 네임스페이스 필터 필드.
pub const FIELD_ONLY: &str = "only";
/// destination 선택 필드.
pub const FIELD_FROM: &str = "from";
/// 승인 요청이 되돌려주는 계획 지문 필드.
pub const FIELD_FINGERPRINT: &str = "plan_fingerprint";

/// `restore --json`이 낼 스키마 버전의 웹 쪽 사본.
const EXPECTED_RESTORE_SCHEMA: u32 = 1;

/// dry-run 실행 상한 — 양쪽 서버 접속과 카탈로그 훑기가 낀다.
const PLAN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// 결과 화면 링크.
pub fn restore_result_href(id: &JobId) -> String {
    format!("/restore/job/{id}")
}

// ---------------------------------------------------------------------------
// `--at` 사전 검증
// ---------------------------------------------------------------------------

/// `--at` 입력이 거부된 이유.
///
/// oplog 윈도우 밖은 여기 없다 — 그건 저장소를 봐야 알 수 있어 자식이 잡는다(모듈 헤더).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AtProblem {
    /// 타임존(오프셋)이 없다 — 가장 흔한 실수라 별도 변종으로 둔다.
    MissingTimezone,
    /// RFC 3339로 읽을 수 없다.
    Malformed,
    /// 지금보다 뒤의 시각이다.
    InFuture {
        /// 요청된 시각(정규화된 표기).
        requested: String,
    },
}

impl AtProblem {
    /// 화면에 그대로 실리는, **무엇을 고쳐야 하는지 말하는** 문장.
    pub fn explain(&self, lang: crate::i18n::Lang) -> String {
        match self {
            AtProblem::MissingTimezone => lang.sel(
                "The point in time needs a timezone. Add a trailing Z for UTC — for example 2026-06-12T13:00:00Z — or an offset like +09:00.",
                "복구 시점에 타임존이 없습니다. UTC라면 끝에 Z를 붙이세요 — 예: 2026-06-12T13:00:00Z — 또는 +09:00 같은 오프셋을 적으세요.",
            ).to_string(),
            AtProblem::Malformed => lang.sel(
                "The point in time is not a valid RFC 3339 timestamp. It looks like 2026-06-12T13:00:00Z — year-month-day, then T, then hours:minutes:seconds, then the timezone.",
                "복구 시점이 올바른 RFC 3339 시각이 아닙니다. 형식은 2026-06-12T13:00:00Z입니다 — 연-월-일, T, 시:분:초, 그다음 타임존.",
            ).to_string(),
            AtProblem::InFuture { requested } => format!(
                "{} ({requested})",
                lang.sel(
                    "That point in time is in the future. A restore can only replay what has already been recorded.",
                    "그 시점은 미래입니다. 복구는 이미 기록된 것까지만 재생할 수 있습니다.",
                )
            ),
        }
    }
}

/// `--at` 문자열을 검증한다 — **자식을 띄우지 않는 순수 함수**.
///
/// `now`를 인자로 받는 이유는 [`crate::web::auth::AuthState::note_failure`]와 같다:
/// "미래 시각" 판정을 실제 시계 없이 테스트할 수 있어야 한다.
///
/// 타임존 없음을 형식 오류와 가르는 방법: RFC 3339 파싱이 실패한 뒤, **같은 문자열이
/// 타임존만 빼면 유효한지**를 확인한다. 유효하면 사람이 빠뜨린 것은 타임존 하나뿐이고,
/// 그때 필요한 문장은 "형식이 틀렸다"가 아니라 "Z를 붙이세요"다.
pub fn validate_at(raw: &str, now: DateTime<Utc>) -> std::result::Result<DateTime<Utc>, AtProblem> {
    let trimmed = raw.trim();

    let parsed = match DateTime::parse_from_rfc3339(trimmed) {
        Ok(dt) => dt.with_timezone(&Utc),
        Err(_) => {
            // 타임존만 빠진 것인가?
            let naive_ok = chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S")
                .is_ok()
                || chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
                || chrono::NaiveDateTime::parse_from_str(trimmed, "%Y-%m-%d %H:%M:%S").is_ok();
            return Err(if naive_ok {
                AtProblem::MissingTimezone
            } else {
                AtProblem::Malformed
            });
        }
    };

    if parsed > now {
        return Err(AtProblem::InFuture {
            requested: parsed.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        });
    }

    Ok(parsed)
}

// ---------------------------------------------------------------------------
// 요청 값
// ---------------------------------------------------------------------------

/// 폼에서 읽은 복구 요청.
#[derive(Debug, Clone)]
struct RestoreRequest {
    profile: ProfileName,
    /// 백업 id(선택) — 없으면 최신 풀백업을 자식이 고른다.
    id: Option<String>,
    /// PITR 시점(선택). 검증을 통과한 원문을 그대로 들고 있다.
    at: Option<String>,
    /// 복구 대상(없으면 in-place).
    target_profile: Option<ProfileName>,
    only: Option<String>,
    from: Option<String>,
}

impl RestoreRequest {
    /// 폼에서 읽고 검증한다.
    ///
    /// `--id`와 `--at`을 **동시에** 받지 않는다. CLI는 둘 다 받아 `--at`을 우선하지만,
    /// 웹 화면에서 두 칸을 모두 채운 사람은 둘 중 무엇이 이길지 모른 채 승인하게 된다 —
    /// 복구 화면에서 "무엇이 실행되는지 모르는 승인"은 만들지 않는다.
    fn from_form(
        form: &FormBody,
        now: DateTime<Utc>,
        lang: Lang,
    ) -> std::result::Result<Self, String> {
        let profile = ProfileName::parse(form.get(FIELD_PROFILE).unwrap_or_default(), lang)
            .map_err(|e| format!("profile: {e}"))?;

        let id = form.non_empty(FIELD_ID).map(str::to_string);
        let at_raw = form.non_empty(FIELD_AT).map(str::to_string);

        if id.is_some() && at_raw.is_some() {
            return Err(
                "백업 id와 복구 시점을 동시에 지정할 수 없습니다 — 둘 중 하나만 채우세요."
                    .to_string(),
            );
        }

        // 시점이 있으면 **자식을 띄우기 전에** 검증한다(모듈 헤더).
        if let Some(raw) = &at_raw {
            validate_at(raw, now).map_err(|p| p.explain(crate::i18n::Lang::Ko))?;
        }

        let target_profile = match form.non_empty(FIELD_TARGET_PROFILE) {
            Some(name) => Some(ProfileName::parse(name, lang).map_err(|e| format!("target: {e}"))?),
            None => None,
        };

        Ok(Self {
            profile,
            id,
            at: at_raw,
            target_profile,
            only: form.non_empty(FIELD_ONLY).map(str::to_string),
            from: form.non_empty(FIELD_FROM).map(str::to_string),
        })
    }

    /// 대상 출처.
    fn origin(&self) -> TargetOrigin {
        match &self.target_profile {
            Some(name) => TargetOrigin::Profile(name.as_str().to_string()),
            None => TargetOrigin::InPlace,
        }
    }

    /// **확인에서 이름을 타이핑하게 할 프로파일.**
    ///
    /// 파괴적 효과가 일어나는 쪽이다(`routes::migrate`와 같은 판단) — 대상을 따로 골랐으면
    /// 그것, in-place면 프로파일 자신.
    fn destructive_target(&self) -> &ProfileName {
        self.target_profile.as_ref().unwrap_or(&self.profile)
    }

    /// 확인 폼에 되실을 값들.
    fn as_hidden(&self, fingerprint: &str) -> Vec<(String, String)> {
        let mut fields = vec![
            (FIELD_PROFILE.to_string(), self.profile.as_str().to_string()),
            (FIELD_FINGERPRINT.to_string(), fingerprint.to_string()),
        ];
        for (name, value) in [
            (FIELD_ID, self.id.as_ref()),
            (FIELD_AT, self.at.as_ref()),
            (FIELD_ONLY, self.only.as_ref()),
            (FIELD_FROM, self.from.as_ref()),
        ] {
            if let Some(value) = value {
                fields.push((name.to_string(), value.clone()));
            }
        }
        if let Some(target) = &self.target_profile {
            fields.push((
                FIELD_TARGET_PROFILE.to_string(),
                target.as_str().to_string(),
            ));
        }
        fields
    }

    /// 이 잡이 상태를 바꾸는 프로파일들 — 캐시 무효화 대상.
    ///
    /// in-place면 프로파일 하나, 대상을 따로 골랐으면 둘 다 넣는다(읽는 쪽도 부하가
    /// 달라진다 — `routes::migrate::touched_profiles`와 같은 판단).
    fn touched_profiles(&self) -> Vec<String> {
        let mut names = vec![self.profile.as_str().to_string()];
        if let Some(target) = &self.target_profile {
            names.push(target.as_str().to_string());
        }
        names
    }

    /// `JobSpec` 뼈대.
    fn base_spec(&self, lang: Lang) -> std::result::Result<JobSpec, String> {
        let mut spec = JobSpec::new(JobCommand::Restore, lang).with_profile(self.profile.clone());
        if let Some(target) = &self.target_profile {
            spec = spec.with_target_profile(target.clone());
        }
        if let Some(id) = &self.id {
            spec = spec.with_backup_id(id).map_err(|e| e.detail())?;
        }
        if let Some(at) = &self.at {
            spec = spec.with_at(at).map_err(|e| e.detail())?;
        }
        if let Some(only) = &self.only {
            spec = spec.with_only(only).map_err(|e| e.detail())?;
        }
        if let Some(from) = &self.from {
            spec = spec.with_from(from).map_err(|e| e.detail())?;
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
    /// stdout이 비었다 — 자식이 계획을 내기 전에 끝났다(윈도우 밖 거부가 여기로 온다).
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
                    "restore produced no plan. Read the diagnostics below — a point in time outside the recoverable window ends here.",
                    "restore가 계획을 내지 않았습니다. 아래 진단을 보세요 — 복구 가능 범위 밖의 시점이 이 경로로 끝납니다.",
                )
                .to_string(),
            PlanError::Malformed(detail) => format!(
                "{} {detail}",
                lang.sel(
                    "restore output could not be parsed as the expected JSON:",
                    "restore 출력을 기대한 JSON으로 해석할 수 없습니다:",
                )
            ),
            PlanError::SchemaMismatch { found, expected } => format!(
                "{} (schema {found} \u{2260} {expected})",
                lang.sel(
                    "restore reported a JSON schema this console does not know — the console and the CLI are probably different builds.",
                    "restore가 이 콘솔이 모르는 JSON 스키마를 냈습니다 — 콘솔과 CLI가 서로 다른 빌드일 가능성이 큽니다.",
                )
            ),
            PlanError::TooDeep { found, max } => format!(
                "{} ({found} > {max})",
                lang.sel(
                    "restore output is nested more deeply than this console parses.",
                    "restore 출력의 중첩 깊이가 이 콘솔이 파싱하는 상한을 넘었습니다.",
                )
            ),
        }
    }
}

/// 자식 stdout을 계획으로 접는다 — **순수 함수**.
///
/// 풀 복구와 PITR은 **다른 모양**의 문서를 낸다(`cli/handlers/restore.rs`의
/// `build_plan_json` vs `build_pitr_plan_json`). 전자는 `backup_id`를, 후자는 `base_id`와
/// `decided_wall_clock`을 가진다 — 그 존재로 가른다.
pub fn parse_plan(
    stdout: &str,
    request_profile: &str,
    origin: TargetOrigin,
    requested_at: Option<&str>,
    only: Option<&str>,
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
        Some(found) if found == u64::from(EXPECTED_RESTORE_SCHEMA) => {}
        Some(found) => {
            return Err(PlanError::SchemaMismatch {
                found,
                expected: EXPECTED_RESTORE_SCHEMA,
            })
        }
        None => {
            return Err(PlanError::Malformed(
                "최상위 `schema` 필드가 없습니다".to_string(),
            ))
        }
    }

    if value.get("dry_run").and_then(serde_json::Value::as_bool) != Some(true) {
        return Err(PlanError::Malformed(
            "dry-run 계획이 아닙니다(`dry_run`이 true가 아님)".to_string(),
        ));
    }

    let destination = string_at(&value, "destination");

    // PITR 계획인가 — `base_id`가 있으면 그렇다.
    let pitr = value
        .get("base_id")
        .and_then(serde_json::Value::as_str)
        .map(|base| {
            let replay = value
                .get("incremental_ids")
                .and_then(serde_json::Value::as_array)
                .map_or(0, Vec::len);
            (
                base.to_string(),
                view::PitrPoint {
                    requested: requested_at.unwrap_or_default().to_string(),
                    decided: string_at(&value, "decided_wall_clock"),
                    replay_slices: replay,
                },
            )
        });

    let (backup_id, pitr_point) = match pitr {
        Some((base, point)) => (base, Some(point)),
        None => (string_at(&value, "backup_id"), None),
    };

    let conflicting: Vec<String> = value
        .get("conflicting_namespaces")
        .and_then(serde_json::Value::as_array)
        .map(|a| {
            a.iter()
                .map(|v| v.as_str().unwrap_or_default().to_string())
                .collect()
        })
        .unwrap_or_default();

    let fingerprint = plan_fingerprint(
        &backup_id,
        pitr_point.as_ref().map(|p| p.decided.as_str()),
        &destination,
        &conflicting,
    );

    Ok(view::Plan {
        profile: request_profile.to_string(),
        destination,
        origin,
        backup_id,
        pitr: pitr_point,
        conflicting,
        version_warning: value
            .get("version_warning")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        only: only.map(str::to_string),
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

/// 계획의 지문.
///
/// 운영자가 승인하는 명제는 "**이 백업(또는 이 시점)을 이 대상에 복구하고, 이것들을
/// 덮어쓴다**"이다. 그래서 그 넷을 반영한다. 크기·버전은 넣지 않는다 — 달라져도 승인의
/// 전제가 바뀌지 않는다(`routes::migrate`의 문서 수와 같은 판단).
pub fn plan_fingerprint(
    backup_id: &str,
    decided: Option<&str>,
    destination: &str,
    conflicting: &[String],
) -> String {
    let mut conflicts: Vec<&str> = conflicting.iter().map(String::as_str).collect();
    conflicts.sort_unstable();

    let mut hasher = Sha256::new();
    hasher.update(backup_id.as_bytes());
    hasher.update([0x1f]);
    hasher.update(decided.unwrap_or_default().as_bytes());
    hasher.update([0x1f]);
    hasher.update(destination.as_bytes());
    hasher.update([0x1d]);
    for ns in conflicts {
        hasher.update(ns.as_bytes());
        hasher.update([0x1f]);
    }
    format!("{:x}", hasher.finalize())
}

// ---------------------------------------------------------------------------
// 핸들러
// ---------------------------------------------------------------------------

/// `GET /restore` 쿼리 — 카탈로그에서 `?id=`로 넘어온다.
#[derive(Debug, Default, serde::Deserialize)]
pub struct FormQuery {
    /// 프리필할 백업 id.
    id: Option<String>,
    /// 프리필할 프로파일.
    profile: Option<String>,
}

/// `GET /restore` — 폼.
pub async fn form(State(ctx): State<Arc<ServeConfig>>, Query(query): Query<FormQuery>) -> Markup {
    let profiles = load_profile_names(&ctx);
    let prefill = Prefill {
        profile: query.profile,
        id: query.id,
        ..Prefill::default()
    };
    layout::shell(
        ctx.lang,
        RESTORE_TITLE,
        view::form_body(ctx.lang, &profiles, &prefill, None),
    )
}

/// `POST /restore/plan` — dry-run으로 계획을 받아 확인 화면과 함께 보여준다.
pub async fn plan(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    let request = match RestoreRequest::from_form(&form_body, Utc::now(), ctx.lang) {
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

/// `POST /restore/apply` — 확인을 검증하고 실제로 복구한다.
pub async fn apply(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    let form_body = FormBody::parse(&body);

    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }

    if form_body
        .get(guard::FIELD_CONFIRM_TOKEN)
        .is_none_or(str::is_empty)
    {
        return refused(
            &ctx,
            ctx.lang.sel(
                "This request did not come from a confirmation screen. Preview the restore first, then approve it there.",
                "이 요청은 확인 화면에서 온 것이 아닙니다. 먼저 계획을 미리 보고 그 화면에서 승인하세요.",
            ),
        );
    }

    // `--at`은 승인 요청에서도 다시 검증한다 — hidden 필드는 확인 토큰이 보호하지 않으므로
    // 되돌아온 값을 신뢰하지 않는다(`guard::DestructiveRequest::extra_hidden` doc).
    let request = match RestoreRequest::from_form(&form_body, Utc::now(), ctx.lang) {
        Ok(r) => r,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };

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

    let allow_overwrite = form_body.checked(guard::FIELD_ALLOW_OVERWRITE);
    let submission = ConfirmSubmission {
        token: form_body.get(guard::FIELD_CONFIRM_TOKEN),
        typed_name: form_body.get(guard::FIELD_CONFIRM_NAME).unwrap_or_default(),
        allow_overwrite,
    };
    let what = DestructiveTarget {
        action: JobCommand::Restore.audit_action(),
        target: request.destructive_target(),
    };
    let secrets = ctx.jobs.secret_registry().clone();
    let spec_for_args = match build_spec(&request, allow_overwrite, ctx.lang) {
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

    let spec = match build_spec(&request, gate.allow_overwrite, ctx.lang) {
        Ok(spec) => spec,
        Err(message) => {
            return form_with_notice(&ctx, StatusCode::BAD_REQUEST, &form_body, &message)
        }
    };
    match start_restore_job(&ctx, spec, gate.receipt, request.touched_profiles()).await {
        Ok(id) => (
            StatusCode::SEE_OTHER,
            [(header::LOCATION, restore_result_href(&id))],
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

/// `GET /restore/job/{job_id}` — 결과. 잡 이력 화면으로 넘긴다.
pub async fn result(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let id = match JobId::parse(&raw) {
        Ok(id) => id,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                layout::shell(ctx.lang, RESTORE_TITLE, view::malformed_id(ctx.lang)),
            )
                .into_response()
        }
    };
    let detail = JobStore::attach(&ctx.state_dir).detail(&id).await;
    if detail.summary.is_none() && !detail.log_file_present {
        return (
            StatusCode::NOT_FOUND,
            layout::shell(ctx.lang, RESTORE_TITLE, view::unknown_job(ctx.lang)),
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
            RESTORE_TITLE,
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
    let prefill = Prefill {
        profile: form_body.get(FIELD_PROFILE).map(str::to_string),
        id: form_body.non_empty(FIELD_ID).map(str::to_string),
        at: form_body.non_empty(FIELD_AT).map(str::to_string),
        target_profile: form_body
            .non_empty(FIELD_TARGET_PROFILE)
            .map(str::to_string),
        only: form_body.non_empty(FIELD_ONLY).map(str::to_string),
    };
    (
        status,
        layout::shell(
            ctx.lang,
            RESTORE_TITLE,
            view::form_body(
                ctx.lang,
                &profiles,
                &prefill,
                Some(view::validation_notice(ctx.lang, message)),
            ),
        ),
    )
        .into_response()
}

async fn render_plan(
    ctx: &Arc<ServeConfig>,
    request: &RestoreRequest,
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
    (StatusCode::OK, layout::shell(ctx.lang, RESTORE_TITLE, body)).into_response()
}

fn confirm_request<'a>(
    lang: crate::i18n::Lang,
    request: &'a RestoreRequest,
    plan: &view::Plan,
) -> DestructiveRequest<'a> {
    let mut summary: Vec<(&'static str, String)> = vec![
        ("command", "restore".to_string()),
        ("restore into", plan.destination.clone()),
        ("origin", plan.origin.label(lang)),
        ("backup", plan.backup_id.clone()),
    ];
    if let Some(pitr) = &plan.pitr {
        summary.push(("point in time", pitr.decided.clone()));
    }
    if let Some(only) = &plan.only {
        summary.push(("only", only.clone()));
    }
    if plan.has_conflicts() {
        summary.push(("existing namespaces", plan.conflicting.len().to_string()));
    }

    DestructiveRequest {
        what: DestructiveTarget {
            action: JobCommand::Restore.audit_action(),
            target: request.destructive_target(),
        },
        headline: if plan.origin.is_in_place() {
            lang.sel(
                "This writes back into the profile's own source",
                "이 작업은 프로파일 자신의 source에 되씁니다",
            )
        } else {
            lang.sel(
                "This writes into a live server",
                "이 작업은 살아 있는 서버에 씁니다",
            )
        }
        .to_string(),
        irreversible_notice: lang.sel(
            "Whatever is on the target now is replaced by the backup's contents. This console cannot undo it — getting the target back to its current state would need a backup taken right now.",
            "지금 대상에 있는 내용이 백업의 내용으로 대체됩니다. 이 콘솔로는 되돌릴 수 없습니다 — 현재 상태로 돌아가려면 지금 이 순간의 백업이 필요합니다.",
        ).to_string(),
        summary,
        overwrite: plan.has_conflicts().then(|| guard::OverwriteOption {
            label: lang
                .sel(
                    "Allow overwriting the existing data (--force)",
                    "기존 데이터 덮어쓰기를 허용(--force)",
                )
                .to_string(),
            hint: lang.sel(
                "Off by default, matching the CLI's guardrail. Without it the restore refuses when the target already holds data.",
                "CLI의 가드레일과 같이 기본은 꺼짐입니다. 켜지 않으면 대상에 데이터가 있을 때 복구가 거부됩니다.",
            ).to_string(),
        }),
        extra_hidden: request.as_hidden(&plan.fingerprint),
        submit_path: RESTORE_APPLY_PATH,
        submit_label: lang
            .sel("Restore into the target", "대상으로 복구")
            .to_string(),
        cancel_href: RESTORE_PATH,
    }
}

/// 실행용 `JobSpec`.
fn build_spec(
    request: &RestoreRequest,
    allow_overwrite: bool,
    lang: Lang,
) -> std::result::Result<JobSpec, String> {
    let mut spec = request.base_spec(lang)?;
    if allow_overwrite {
        spec = spec.with_flag(JobFlag::Force);
    }
    Ok(spec)
}

/// dry-run 자식을 돌려 계획을 받아온다.
async fn run_dry_run(
    ctx: &Arc<ServeConfig>,
    request: &RestoreRequest,
) -> std::result::Result<view::Plan, String> {
    let spec = request.base_spec(ctx.lang)?.with_flag(JobFlag::DryRun);

    let running = ctx.jobs.spawn_preview(&spec).map_err(|e| e.to_string())?;
    let completion = tokio::time::timeout(PLAN_TIMEOUT, running.wait_with_output())
        .await
        .map_err(|_| {
            ctx.lang
                .sel(
                    "restore --dry-run did not finish in time.",
                    "restore --dry-run이 제한 시간 안에 끝나지 않았습니다.",
                )
                .to_string()
        })?
        .map_err(|e| e.to_string())?;

    let registry = ctx.jobs.secret_registry();
    parse_plan(
        &completion.stdout,
        request.profile.as_str(),
        request.origin(),
        request.at.as_deref(),
        request.only.as_deref(),
    )
    .map_err(|e| {
        // 계획이 없으면 자식의 진단(윈도우 밖 거부 등)을 함께 보여준다 — 그 문장이
        // "왜 안 되는지"를 아는 유일한 곳이다. 마스킹을 반드시 거친다.
        let stderr = registry.mask(completion.stderr.trim());
        if stderr.is_empty() {
            e.explain(ctx.lang)
        } else {
            format!("{} — {stderr}", e.explain(ctx.lang))
        }
    })
}

/// 확정된 복구 잡을 띄우고 이력에 기록한다.
async fn start_restore_job(
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
                tracing::warn!(job_id = %job_id, error = %e, "restore 자식 대기 실패");
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
            tracing::warn!(job_id = %job_id, error = %e, "restore 종료 기록 실패");
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
            tracing::warn!(job_id = %job_id, error = %e, "restore 완료 감사 기록 실패");
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

    const TEST_TOKEN: &str = "test-token-restore-5b7e13";
    const PROFILE: &str = "prod";
    const TARGET: &str = "dr";
    /// 백업 id는 16진수와 `-`만 허용된다(`JobSpec::with_backup_id`) — UUID 모양을 쓴다.
    const BACKUP_ID: &str = "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b";

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-06-12T12:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }

    // ---- `--at` 사전 검증: done_criteria의 4케이스 ----

    /// **1) 형식 오류** — 자식 없이 거부되고, 형식을 알려준다.
    #[test]
    fn a_malformed_timestamp_is_rejected_with_the_expected_shape() {
        for bad in ["어제", "2026/06/12", "12:00", "not a time", ""] {
            assert_eq!(
                validate_at(bad, now()),
                Err(AtProblem::Malformed),
                "{bad:?}가 형식 오류로 잡히지 않았다"
            );
        }
        let message = AtProblem::Malformed.explain(Lang::En);
        assert!(
            message.contains("2026-06-12T13:00:00Z"),
            "올바른 형식 예시가 없다: {message}"
        );
    }

    /// **2) 타임존 없음** — 형식 오류와 **다른** 메시지가 나온다. 가장 흔한 실수라
    /// "Z를 붙이세요"라고 말해야 한다.
    #[test]
    fn a_missing_timezone_gets_its_own_message() {
        for naive in [
            "2026-06-12T13:00:00",
            "2026-06-12T13:00:00.123",
            "2026-06-12 13:00:00",
        ] {
            assert_eq!(
                validate_at(naive, now()),
                Err(AtProblem::MissingTimezone),
                "{naive:?}가 타임존 누락으로 잡히지 않았다"
            );
        }

        let missing = AtProblem::MissingTimezone.explain(Lang::En);
        let malformed = AtProblem::Malformed.explain(Lang::En);
        assert_ne!(missing, malformed, "두 오류가 같은 문장을 낸다");
        assert!(missing.contains('Z'), "Z를 붙이라는 안내가 없다: {missing}");
    }

    /// **3) 미래 시각** — 자식 없이 거부된다.
    #[test]
    fn a_future_timestamp_is_rejected() {
        let future = "2026-06-12T12:00:01Z";
        assert_eq!(
            validate_at(future, now()),
            Err(AtProblem::InFuture {
                requested: "2026-06-12T12:00:01Z".to_string()
            })
        );

        // 경계: 지금과 같은 시각은 미래가 아니다.
        assert!(validate_at("2026-06-12T12:00:00Z", now()).is_ok());
    }

    /// 정상 입력은 통과하고 UTC로 정규화된다(오프셋 표기도 받는다).
    #[test]
    fn valid_timestamps_pass_and_normalize_to_utc() {
        let utc = validate_at("2026-06-12T11:00:00Z", now()).expect("UTC가 거부됐다");
        let offset = validate_at("2026-06-12T20:00:00+09:00", now()).expect("오프셋이 거부됐다");
        assert_eq!(utc, offset, "같은 순간이 다르게 해석됐다");

        // 앞뒤 공백은 흔한 붙여넣기 사고다 — 값 자체가 유효하면 통과시킨다.
        assert!(validate_at("  2026-06-12T11:00:00Z  ", now()).is_ok());
    }

    /// **4) oplog 윈도우 밖**은 여기서 잡지 않는다 — 자식이 낸 진단이 화면에 실린다.
    /// 그 경로를 [`PlanError::Empty`]가 담당한다는 것을 문장으로 고정한다.
    #[test]
    fn an_out_of_window_point_is_explained_as_a_plan_failure() {
        let message = PlanError::Empty.explain(Lang::En);
        assert!(
            message.contains("recoverable window"),
            "윈도우 밖 거부가 이 경로로 온다는 것을 말하지 않는다: {message}"
        );
    }

    // ---- 폼 검증 ----

    #[test]
    fn an_id_and_a_point_in_time_cannot_both_be_given() {
        let form = FormBody::parse(&format!(
            "{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_AT}=2026-06-12T11:00:00Z"
        ));
        let err =
            RestoreRequest::from_form(&form, now(), Lang::En).expect_err("둘 다 지정이 통과했다");
        assert!(err.contains("동시에"), "무엇이 문제인지 말해야 한다: {err}");
    }

    #[test]
    fn the_form_rejects_a_bad_point_in_time_before_anything_else() {
        let form = FormBody::parse(&format!(
            "{FIELD_PROFILE}={PROFILE}&{FIELD_AT}=2026-06-12T13:00:00"
        ));
        let err = RestoreRequest::from_form(&form, now(), Lang::En)
            .expect_err("타임존 없는 값이 통과했다");
        assert!(err.contains('Z'), "고치는 법을 말해야 한다: {err}");
    }

    /// **이름을 타이핑하게 할 대상은 파괴적 효과가 일어나는 쪽이다.**
    #[test]
    fn the_destructive_target_follows_where_the_write_lands() {
        let in_place = FormBody::parse(&format!("{FIELD_PROFILE}={PROFILE}"));
        let request = RestoreRequest::from_form(&in_place, now(), Lang::En).unwrap();
        assert_eq!(request.destructive_target().as_str(), PROFILE);
        assert!(request.origin().is_in_place());

        let elsewhere = FormBody::parse(&format!(
            "{FIELD_PROFILE}={PROFILE}&{FIELD_TARGET_PROFILE}={TARGET}"
        ));
        let request = RestoreRequest::from_form(&elsewhere, now(), Lang::En).unwrap();
        assert_eq!(
            request.destructive_target().as_str(),
            TARGET,
            "대상을 골랐는데 source 이름을 묻는다"
        );
        assert_eq!(request.origin(), TargetOrigin::Profile(TARGET.to_string()));
    }

    // ---- 계획 파싱 ----

    #[test]
    fn a_full_restore_plan_is_read_without_a_pitr_section() {
        let json = r#"{"schema":1,"dry_run":true,"backup_id":"bk-1","backup_type":"Full",
"destination":"mongodb://***:***@db:27017","source_server_version":"7.0.5",
"target_server_version":"7.0.5","stored_size_bytes":123,"ns_include":null,
"conflicting_namespaces":["shop.orders"],"version_warning":null}"#;
        let plan = parse_plan(json, PROFILE, TargetOrigin::InPlace, None, None).expect("파싱 실패");
        assert_eq!(plan.backup_id, "bk-1");
        assert!(plan.pitr.is_none(), "풀 복구인데 PITR 정보가 붙었다");
        assert_eq!(plan.destination, "mongodb://***:***@db:27017");
        assert_eq!(plan.conflicting, vec!["shop.orders".to_string()]);
    }

    /// PITR 계획은 `base_id`로 가려지고, 요청 시각과 도달 시각을 함께 담는다.
    #[test]
    fn a_pitr_plan_carries_both_moments() {
        let json = r#"{"schema":1,"dry_run":true,"base_id":"base-1",
"destination":"mongodb://***@db:27017","incremental_ids":["i1","i2"],
"target_unix":1780000000,"decided_ts":{"t":1779999953,"i":1},
"decided_wall_clock":"2026-06-12T12:59:13Z","limit_slice_id":"i2","estimated_bytes":42}"#;
        let plan = parse_plan(
            json,
            PROFILE,
            TargetOrigin::InPlace,
            Some("2026-06-12T13:00:00Z"),
            None,
        )
        .expect("파싱 실패");

        assert_eq!(plan.backup_id, "base-1", "base가 복구 기준이다");
        let pitr = plan.pitr.expect("PITR 정보가 없다");
        assert_eq!(pitr.requested, "2026-06-12T13:00:00Z");
        assert_eq!(pitr.decided, "2026-06-12T12:59:13Z");
        assert_eq!(pitr.replay_slices, 2);
    }

    #[test]
    fn a_completion_summary_is_not_accepted_as_a_plan() {
        let summary = r#"{"schema":1,"backup_id":"bk-1","stored_size_bytes":1,
"destination":"d","restored":true}"#;
        assert!(matches!(
            parse_plan(summary, PROFILE, TargetOrigin::InPlace, None, None).unwrap_err(),
            PlanError::Malformed(_)
        ));
    }

    #[test]
    fn plan_errors_cover_empty_broken_and_foreign_schemas() {
        let e =
            |json: &str| parse_plan(json, PROFILE, TargetOrigin::InPlace, None, None).unwrap_err();
        assert_eq!(e(""), PlanError::Empty);
        assert!(matches!(e("not json"), PlanError::Malformed(_)));
        assert_eq!(
            e(r#"{"schema":99,"dry_run":true}"#),
            PlanError::SchemaMismatch {
                found: 99,
                expected: EXPECTED_RESTORE_SCHEMA
            }
        );
        assert!(matches!(e(&"[".repeat(50_000)), PlanError::TooDeep { .. }));
    }

    /// 지문은 **승인의 전제**(어느 백업/시점을 어디에 복구하고 무엇을 덮어쓰는가)만 반영한다.
    #[test]
    fn the_fingerprint_tracks_what_was_approved() {
        let base = plan_fingerprint("bk-1", None, "dest", &[]);
        assert_ne!(
            base,
            plan_fingerprint("bk-2", None, "dest", &[]),
            "백업이 달라졌다"
        );
        assert_ne!(
            base,
            plan_fingerprint("bk-1", None, "other", &[]),
            "대상이 달라졌다"
        );
        assert_ne!(
            base,
            plan_fingerprint("bk-1", Some("2026-01-01T00:00:00Z"), "dest", &[]),
            "시점이 붙었다"
        );
        assert_ne!(
            base,
            plan_fingerprint("bk-1", None, "dest", &["a.b".to_string()]),
            "덮어쓸 것이 생겼다"
        );
        // 충돌 순서는 무관하다.
        assert_eq!(
            plan_fingerprint(
                "bk-1",
                None,
                "dest",
                &["a.b".to_string(), "c.d".to_string()]
            ),
            plan_fingerprint(
                "bk-1",
                None,
                "dest",
                &["c.d".to_string(), "a.b".to_string()]
            )
        );
    }

    // ---- 라우트 E2E ----

    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    fn session() -> &'static str {
        &auth_and_session().1
    }

    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(RESTORE_PATH, get(form))
            .route(RESTORE_PLAN_PATH, post(plan))
            .route(RESTORE_APPLY_PATH, post(apply))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .layer(Extension(Arc::clone(&auth_and_session().0)))
            .with_state(ctx)
    }

    #[cfg(unix)]
    fn fake_exe(dir: &FsPath) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-x-backup");
        // `plan.json`이 없으면 stderr로 진단만 내고 실패한다 — oplog 윈도우 밖 거부를
        // 흉내내는 경로다.
        let script = r#"#!/bin/sh
dir="$(dirname "$0")"
dry=0
for a in "$@"; do
  case "$a" in --dry-run) dry=1 ;; esac
done
if [ "$dry" = "1" ]; then
  if [ -f "$dir/plan.json" ]; then
    cat "$dir/plan.json"
  else
    echo "복구 가능 범위 밖입니다: 그 시점 이하의 적격 base가 없습니다" >&2
    exit 1
  fi
else
  echo "run $*" >> "$dir/applied"
  echo '{"schema":1,"backup_id":"bk-1","stored_size_bytes":1,"destination":"d","restored":true}'
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
            "x-backup-restore-{tag}-{}-{}",
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
        for profile in [PROFILE, TARGET] {
            toml.push_str(&format!(
                "[profiles.{profile}.source]\nuri_env = \"XB_RESTORE_TEST_{profile}\"\n\n\
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

    fn plant_full_plan(dir: &FsPath, conflicts: &[&str]) {
        let items: Vec<String> = conflicts.iter().map(|c| format!("\"{c}\"")).collect();
        std::fs::write(
            dir.join("plan.json"),
            format!(
                r#"{{"schema":1,"dry_run":true,"backup_id":"bk-1","backup_type":"Full",
"destination":"mongodb://***:***@db.internal:27017","source_server_version":"7.0.5",
"target_server_version":"7.0.5","stored_size_bytes":123,"ns_include":null,
"conflicting_namespaces":[{}],"version_warning":null}}"#,
                items.join(",")
            ),
        )
        .expect("계획 표본 쓰기 실패");
    }

    fn plant_pitr_plan(dir: &FsPath) {
        std::fs::write(
            dir.join("plan.json"),
            r#"{"schema":1,"dry_run":true,"base_id":"base-1",
"destination":"mongodb://***@db.internal:27017","incremental_ids":["i1","i2"],
"target_unix":1780000000,"decided_ts":{"t":1779999953,"i":1},
"decided_wall_clock":"2026-06-12T12:59:13Z","limit_slice_id":"i2","estimated_bytes":42}"#,
        )
        .expect("PITR 계획 표본 쓰기 실패");
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

    /// **E2E 1**: 백업 선택 복구 — 계획 → 승인 → 실제 실행.
    #[cfg(unix)]
    #[tokio::test]
    async fn restoring_a_chosen_backup_runs_after_confirmation() {
        let (ctx, dir) = ctx_with_fake_exe("byid");
        plant_full_plan(&dir, &[]);

        let (status, markup) = post_req(
            Arc::clone(&ctx),
            RESTORE_PLAN_PATH,
            &format!(
                "{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_TARGET_PROFILE}={TARGET}"
            ),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{markup}");
        // done_criteria: 가려진 대상 URI + origin 라벨.
        assert!(
            markup.contains("db.internal:27017"),
            "대상이 표시되지 않았다"
        );
        assert!(markup.contains("***"), "URI가 가려지지 않았다");
        assert!(
            markup.contains("--target-profile: dr"),
            "origin 라벨이 없다"
        );
        assert!(applied(&dir).is_empty(), "계획 단계가 실행했다");

        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        let body = format!(
            "{}={token}&{}={TARGET}&{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), RESTORE_APPLY_PATH, &body, true).await;
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
            argv.contains(&format!("--id {BACKUP_ID}")),
            "백업 id가 argv에 없다: {argv}"
        );
        assert!(
            argv.contains("--target-profile dr"),
            "대상이 argv에 없다: {argv}"
        );
        assert!(
            !argv.contains("--dry-run"),
            "실행에 --dry-run이 남았다: {argv}"
        );
    }

    /// **E2E 2**: `--at` 시점 복구 — 요청/도달 시각이 화면에 뜨고, 승인하면 실행된다.
    #[cfg(unix)]
    #[tokio::test]
    async fn restoring_to_a_point_in_time_runs_after_confirmation() {
        let (ctx, dir) = ctx_with_fake_exe("pitr");
        plant_pitr_plan(&dir);

        let at = "2026-06-12T13:00:00Z";
        let (status, markup) = post_req(
            Arc::clone(&ctx),
            RESTORE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_AT}={at}&{FIELD_TARGET_PROFILE}={TARGET}"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{markup}");
        assert!(markup.contains(at), "요청 시각이 없다");
        assert!(markup.contains("2026-06-12T12:59:13Z"), "도달 시각이 없다");

        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        let body = format!(
            "{}={token}&{}={TARGET}&{FIELD_PROFILE}={PROFILE}&{FIELD_AT}={at}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), RESTORE_APPLY_PATH, &body, true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        for _ in 0..50 {
            if !applied(&dir).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let argv = applied(&dir);
        assert!(argv.contains("--at"), "시점이 argv에 없다: {argv}");
    }

    /// **윈도우 밖**: 자식이 거부하면 확인 화면이 뜨지 않고, 자식의 진단이 보인다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_point_outside_the_window_never_reaches_a_confirmation() {
        let (ctx, dir) = ctx_with_fake_exe("window");
        // plan.json을 심지 않는다 — 가짜 자식이 stderr로 거부한다.

        let (status, markup) = post_req(
            Arc::clone(&ctx),
            RESTORE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_AT}=2020-01-01T00:00:00Z"),
            true,
        )
        .await;

        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert!(
            !markup.contains(guard::FIELD_CONFIRM_TOKEN),
            "거부됐는데 확인 토큰이 발급됐다"
        );
        assert!(
            markup.contains("복구 가능 범위 밖") || markup.contains("recoverable window"),
            "자식의 진단이 화면에 없다: {markup}"
        );
        assert!(applied(&dir).is_empty());
    }

    /// 세 가지 잘못된 `--at`은 **자식을 띄우지 않고** 400으로 끊긴다.
    #[cfg(unix)]
    #[tokio::test]
    async fn bad_timestamps_are_refused_before_any_child_runs() {
        for bad in ["2026-06-12T13:00:00", "어제", "2999-01-01T00:00:00Z"] {
            let (ctx, dir) = ctx_with_fake_exe("badat");
            plant_full_plan(&dir, &[]);

            let (status, _) = post_req(
                Arc::clone(&ctx),
                RESTORE_PLAN_PATH,
                &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_AT}={bad}"),
                true,
            )
            .await;

            assert_eq!(status, StatusCode::BAD_REQUEST, "{bad:?}가 거부되지 않았다");
            assert!(applied(&dir).is_empty());
        }
    }

    /// in-place 복구는 확인에서 **source 프로파일** 이름을 요구한다.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_in_place_restore_asks_for_the_profiles_own_name() {
        let (ctx, dir) = ctx_with_fake_exe("inplace");
        plant_full_plan(&dir, &[]);

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            RESTORE_PLAN_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}"),
            true,
        )
        .await;
        assert!(
            markup.contains("제자리") || markup.contains("in place"),
            "in-place 경고가 없다"
        );

        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);

        // 엉뚱한 이름(dr)은 거부된다.
        let wrong = format!(
            "{}={token}&{}={TARGET}&{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
        );
        let (status, _) = post_req(Arc::clone(&ctx), RESTORE_APPLY_PATH, &wrong, true).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "엉뚱한 이름이 통과했다");
        assert!(applied(&dir).is_empty());
    }

    /// 확인 없이 직접 POST는 403이고 자식을 띄우지 않는다.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_direct_post_is_refused_without_running_anything() {
        let (ctx, dir) = ctx_with_fake_exe("direct");
        plant_full_plan(&dir, &[]);

        let (status, _) = post_req(
            Arc::clone(&ctx),
            RESTORE_APPLY_PATH,
            &format!("{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}"),
            true,
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(applied(&dir).is_empty());
    }

    /// 충돌이 있으면 `--force` 토글이 뜨고, 켰을 때만 argv에 실린다.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_force_toggle_only_appears_with_conflicts_and_must_be_checked() {
        let (ctx, dir) = ctx_with_fake_exe("force");
        plant_full_plan(&dir, &["shop.orders"]);

        let (_, markup) = post_req(
            Arc::clone(&ctx),
            RESTORE_PLAN_PATH,
            &format!(
                "{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_TARGET_PROFILE}={TARGET}"
            ),
            true,
        )
        .await;
        assert!(
            markup.contains(guard::FIELD_ALLOW_OVERWRITE),
            "충돌이 있는데 force 토글이 없다"
        );

        let token = hidden(&markup, guard::FIELD_CONFIRM_TOKEN);
        let fingerprint = hidden(&markup, FIELD_FINGERPRINT);
        let body = format!(
            "{}={token}&{}={TARGET}&{}=1&{FIELD_PROFILE}={PROFILE}&{FIELD_ID}={BACKUP_ID}&{FIELD_TARGET_PROFILE}={TARGET}&{FIELD_FINGERPRINT}={fingerprint}",
            guard::FIELD_CONFIRM_TOKEN,
            guard::FIELD_CONFIRM_NAME,
            guard::FIELD_ALLOW_OVERWRITE,
        );
        let (status, _) = post_req(Arc::clone(&ctx), RESTORE_APPLY_PATH, &body, true).await;
        assert_eq!(status, StatusCode::SEE_OTHER);

        for _ in 0..50 {
            if !applied(&dir).is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            applied(&dir).contains("--force"),
            "토글을 켰는데 --force가 없다"
        );
    }
}
