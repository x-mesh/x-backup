//! `GET /schedule` 계열 — 스케줄 CRUD 화면.
//!
//! ## 이 화면이 콘솔에서 유일하게 하는 일
//! 다른 화면은 대개 `x-backup <cmd> --json` 자식의 출력을 그린다([`crate::web`] 최상위
//! 불변식). 스케줄은 어떤 CLI 명령에도 없는 개념이므로 이 화면은 [`crate::web::routes::jobs`]와
//! 함께 **서버 자신의 상태를 읽고 쓰는** 예외다. 그 예외를 좁히는 규율:
//!
//! - 쓰기는 [`ScheduleStore`]를 경유하고, 그 쓰기 메서드는 [`AuditReceipt`]를 값으로
//!   요구한다 — 감사 기록 없는 변경이 컴파일되지 않는다.
//! - 백업 실행은 여기서 하지 않는다. 스케줄러 루프가
//!   [`crate::web::routes::backup::spawn_tracked_job`]으로만 띄운다.
//! - `crate::engine`·`crate::storage`·`crypto`를 부르지 않는다.
//!
//! ## 감사에 `gate`를 쓰는 이유(`record`가 아니라)
//! [`crate::web::audit`] 헤더는 [`AuditLog::gate`]를 "파괴적 작업 전용"으로 소개하고, receipt를
//! 아무 데도 쓰지 않고 버리는 것은 의미가 없다고 못박는다. [`crate::web::routes::backup`]이
//! `record`를 직접 두 번 부르는 것도 그 이유다(비파괴 명령에는 receipt를 넘길 대상이 없다).
//!
//! 스케줄은 다르다 — **넘길 대상이 있다.** [`ScheduleStore::create`]/`update`/`delete`가
//! receipt를 값으로 받으므로, 여기서 `gate`를 쓰면 receipt가 버려지지 않고 소비된다. 그리고
//! 그 대상이 있어야 하는 이유가 실질적이다: 스케줄 변경은 t27의 config 변경과 같은 성질의
//! 영속 상태 변경이고(그 파일도 `ConfigStore::apply`에 receipt를 넘긴다), 스케줄 삭제는
//! **파일을 하나도 지우지 않으면서 백업을 멈춘다.** 파괴적이 아니라는 것이 기록하지 않아도
//! 된다는 뜻은 아니다.
//!
//! 결과적으로 감사 기록 실패는 **변경 거부**가 된다(`gate`가 `Err`면 저장 함수에 도달하지
//! 못한다). 그게 이 화면에서 옳은 방향이다 — "기록되지 않은 스케줄 변경"은 나중에 백업이
//! 멈춘 원인을 찾을 수 없게 만든다.
//!
//! ## 삭제 확인은 자체 구현이다 — t21의 가드로 승격 대상
//! t21이 `src/web/guard.rs`에 파괴적 작업 공통 가드를 만들고 있다(동시 작업). 그것을 기다리지
//! 않고 최소 확인을 여기서 구현했다: **확인 화면(GET) + 프로파일 이름 타이핑(POST)**. 확인
//! 방식을 [`crate::web::routes::config`]의 프로파일 삭제와 똑같이 맞춘 이유는 일관성이다 —
//! 화면마다 확인 방식이 다르면 운영자가 매번 "이번엔 뭘 적어야 하지"를 생각하고, 그 마찰이
//! 오히려 대충 넘기는 습관을 만든다. **t21의 가드가 들어오면 이 구현을 그것으로 교체해야
//! 한다**(확인 문구·실패 메시지·감사 연동이 한 곳에 모이는 것이 옳다).
//!
//! ## POST 후 리다이렉트하지 않고 결과를 직접 그린다
//! [`crate::web::routes::backup`]은 303으로 리다이렉트하지만(POST-Redirect-GET) 여기는
//! [`crate::web::routes::config`]와 같이 결과 화면을 직접 렌더한다. 근거: 이 화면의 성공 알림은
//! **"다음 실행 시각이 언제인지"**를 담아야 의미가 있고(그게 운영자가 확인하려는 값이다),
//! 리다이렉트하면 그 값을 쿼리 문자열로 실어 보내야 한다.
//!
//! 새로고침으로 재제출되면 어떻게 되는가 — 세 경우 모두 정직한 결과가 나온다:
//! 생성은 중복 거부(400 "같은 스케줄이 이미 있습니다"), 수정은 같은 값으로 다시 저장(성공,
//! 멱등), 삭제는 404("그런 스케줄이 없습니다"). 조용히 두 개가 만들어지는 경우는 없다.
//!
//! ## crontab 조회를 화면 로드마다 하는 이유
//! 목록·생성·수정 화면이 `crontab -l`을 매번 실행한다([`schedule::detect_crontab`]). 기동
//! 시점의 결과를 캐시하지 않는 이유: 운영자가 **지금 막 crontab을 정리하고** 이 화면으로
//! 왔을 수 있고, 그때 낡은 경고가 계속 뜨면 경고 자체를 무시하게 된다. 비용은 로컬 파일을
//! 읽는 자식 하나(수 ms)이고, 3초 타임아웃이 걸려 있어 화면을 붙잡지 않는다.

use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use chrono::Utc;
use maud::Markup;

use crate::error::XBackupError;
use crate::i18n::Lang;
use crate::web::audit::AuditReceipt;
use crate::web::job::args::ProfileName;
use crate::web::mask::SecretRegistry;
use crate::web::schedule::{self, cron::CronExpr};
use crate::web::state::schedules::{self, Schedule, ScheduleDraft, ScheduleId, ScheduleKind};
use crate::web::view::layout;
use crate::web::view::schedule::{self as view, FormMode, FormValues};
use crate::web::ServeConfig;

// ---------------------------------------------------------------------------
// 경로·필드 상수 — 라우터(리더)·마크업·테스트가 공유한다
// ---------------------------------------------------------------------------

/// 목록 화면 경로.
pub const SCHEDULE_PATH: &str = "/schedule";

/// `<title>`·내비게이션 라벨. 기술용어이므로 영문 고정([`crate::i18n`] 규약).
/// 하위 화면(생성·수정·삭제 확인)도 **같은 제목**을 쓴다 — 하나의 내비게이션 항목이다.
pub const SCHEDULE_TITLE: &str = "Schedule";

/// 생성 폼 경로.
pub const NEW_PATH: &str = "/schedule/new";

/// 수정 폼의 **라우터 패턴**(axum 0.8 `{name}` 문법). 링크는 [`edit_href`]로 만든다.
pub const EDIT_ROUTE: &str = "/schedule/edit/{id}";

/// 삭제 확인 화면의 라우터 패턴. 링크는 [`delete_confirm_href`]로 만든다.
pub const DELETE_CONFIRM_ROUTE: &str = "/schedule/delete/{id}";

/// 생성·수정 제출 경로(POST).
pub const SAVE_PATH: &str = "/schedule/save";

/// 삭제 제출 경로(POST).
pub const DELETE_PATH: &str = "/schedule/delete";

/// 폼 필드 이름 — 마크업과 파서가 공유한다.
pub const FIELD_ID: &str = "id";
/// 프로파일 필드.
pub const FIELD_PROFILE: &str = "profile";
/// cron 표현식 필드.
pub const FIELD_EXPR: &str = "expr";
/// 백업 종류 필드.
pub const FIELD_KIND: &str = "kind";
/// 활성 여부 체크박스.
pub const FIELD_ENABLED: &str = "enabled";
/// 삭제 확인 입력.
pub const FIELD_CONFIRM: &str = "confirm";

/// 감사 로그 `actor`. 이 콘솔에는 세션 토큰 하나만 있고 사용자별 신원이 없으므로(t6),
/// 지금 표현할 수 있는 가장 정직한 값은 "웹에서 왔다"는 사실뿐이다.
///
/// **스케줄러가 자동 실행한 잡은 이 값이 아니다** — [`schedule::SCHEDULER_ACTOR`]("scheduler")를
/// 쓴다. 그 구분이 감사 로그에서 "새벽 3시에 누가 백업을 돌렸나"를 답할 수 있게 한다.
const AUDIT_ACTOR: &str = "web";

/// 감사 `action` 어휘 — 고정 문자열이다(감사 로그는 `grep`이 훑는 것을 전제한다).
const ACTION_CREATE: &str = "schedule.create";
/// 수정.
const ACTION_UPDATE: &str = "schedule.update";
/// 삭제.
const ACTION_DELETE: &str = "schedule.delete";

/// 수정 화면 링크. [`ScheduleId`]의 정규형만 들어가므로 이스케이프가 필요한 문자가 없다.
pub fn edit_href(id: ScheduleId) -> String {
    format!("/schedule/edit/{id}")
}

/// 삭제 확인 화면 링크.
pub fn delete_confirm_href(id: ScheduleId) -> String {
    format!("/schedule/delete/{id}")
}

// ---------------------------------------------------------------------------
// 핸들러 — 조회
// ---------------------------------------------------------------------------

/// `GET /schedule` — 목록.
pub async fn list(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    render_list(&ctx, None).await
}

/// `GET /schedule/new` — 생성 폼.
pub async fn new_form(State(ctx): State<Arc<ServeConfig>>) -> Markup {
    let values = FormValues {
        // 새 스케줄은 켜진 상태로 시작한다 — 끈 채로 만들면 "만들었는데 안 돈다"가 되고,
        // 그 사실을 알아차리기 어렵다(이 기능의 핵심 실패 모드).
        enabled: true,
        kind: Some(ScheduleKind::Full),
        ..FormValues::default()
    };
    render_form(&ctx, FormMode::New, &values, None).await
}

/// `GET /schedule/edit/{id}` — 수정 폼.
pub async fn edit_form(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let Some(schedule) = resolve(&ctx, &raw) else {
        return missing(&ctx, &raw);
    };
    let values = FormValues::from_schedule(&schedule);
    let preview = schedule.expr.next_after(Utc::now());
    render_form(&ctx, FormMode::Edit, &values, preview)
        .await
        .into_response()
}

/// `GET /schedule/delete/{id}` — 삭제 확인.
pub async fn delete_form(State(ctx): State<Arc<ServeConfig>>, Path(raw): Path<String>) -> Response {
    let Some(schedule) = resolve(&ctx, &raw) else {
        return missing(&ctx, &raw);
    };
    shell(
        &ctx,
        view::delete_confirm_body(ctx.lang, &schedule, Utc::now(), None),
    )
    .into_response()
}

// ---------------------------------------------------------------------------
// 핸들러 — 변경
// ---------------------------------------------------------------------------

/// `POST /schedule/save` — 생성 또는 수정.
///
/// `id` 필드가 있으면 수정, 없으면 생성이다. 두 경로를 한 엔드포인트로 둔 이유: 검증·감사·
/// 저장 흐름이 완전히 같고, 갈라 두면 그 흐름이 두 벌이 되어 한쪽만 고치는 사고가 난다
/// (t27의 `save`가 같은 판단이다).
///
/// `headers`는 출처 검사용이다(아래 [`reject_foreign_origin`] doc).
pub async fn save(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }
    let form = FormBody::parse(&body);
    let id_raw = form.get(FIELD_ID).filter(|v| !v.is_empty());
    let mode = if id_raw.is_some() {
        FormMode::Edit
    } else {
        FormMode::New
    };
    // 실패해도 사용자가 적은 값이 폼에 남게 미리 담아 둔다(view 헤더 참조).
    let typed = FormValues {
        id: id_raw.map(str::to_string),
        profile: form.get(FIELD_PROFILE).unwrap_or_default().to_string(),
        expr: form.get(FIELD_EXPR).unwrap_or_default().to_string(),
        kind: form
            .get(FIELD_KIND)
            .and_then(|v| ScheduleKind::parse(v).ok()),
        enabled: form.flag(FIELD_ENABLED),
    };

    let draft = match build_draft(&form, ctx.lang) {
        Ok(draft) => draft,
        Err(problem) => return form_problem(&ctx, mode, &typed, &problem).await,
    };

    let (action, target) = (
        if mode == FormMode::Edit {
            ACTION_UPDATE
        } else {
            ACTION_CREATE
        },
        draft.profile.as_str().to_string(),
    );
    let args = audit_args(&draft, &request_registry(&ctx));
    let receipt = match gate(&ctx, action, &target, &args).await {
        Ok(receipt) => receipt,
        Err(problem) => return internal_problem(&ctx, &problem),
    };

    let store = schedules::shared(&ctx.state_dir);
    let saved = match mode {
        FormMode::New => store.create(draft, receipt).await,
        FormMode::Edit => {
            // id는 위에서 존재를 확인했으므로 여기서 파싱 실패는 폼 조작뿐이다.
            let Some(id) = typed
                .id
                .as_deref()
                .and_then(|raw| ScheduleId::parse(raw).ok())
            else {
                return malformed(&ctx);
            };
            store.update(id, draft, receipt).await
        }
    };

    match saved {
        Ok(schedule) => {
            record_outcome(&ctx, action, &target, &args, true).await;
            let notice = view::saved_notice(ctx.lang, schedule.next_run(Utc::now()));
            render_list(&ctx, Some(notice)).await.into_response()
        }
        Err(e) => {
            record_outcome(&ctx, action, &target, &args, false).await;
            match &e {
                // 검증·중복·상한은 사용자가 고칠 수 있는 문제다 — 폼으로 되돌린다.
                XBackupError::Usage(message) => {
                    form_problem(&ctx, mode, &typed, &Problem::bad_request(message.clone())).await
                }
                _ => internal_problem(&ctx, &Problem::internal(e.to_string())),
            }
        }
    }
}

/// `POST /schedule/delete` — 삭제(확인 입력 필수).
pub async fn delete_submit(
    State(ctx): State<Arc<ServeConfig>>,
    headers: HeaderMap,
    body: String,
) -> Response {
    if let Some(rejected) = reject_foreign_origin(&ctx, &headers) {
        return rejected;
    }
    let form = FormBody::parse(&body);
    let Some(id) = form
        .get(FIELD_ID)
        .and_then(|raw| ScheduleId::parse(raw).ok())
    else {
        return malformed(&ctx);
    };
    let store = schedules::shared(&ctx.state_dir);
    let Some(schedule) = store.get(id) else {
        return (
            StatusCode::NOT_FOUND,
            shell(&ctx, view::not_found(ctx.lang)),
        )
            .into_response();
    };

    // 확인 입력은 프로파일 이름과 **정확히** 같아야 한다. 원본으로 비교한다 — 공백을
    // 관대하게 접으면 "이름을 타이핑했다"는 사실 자체가 약해진다(t27과 같은 판단).
    if form.get_raw(FIELD_CONFIRM).unwrap_or("") != schedule.profile {
        let notice = view::problem_notice(
            ctx.lang,
            ctx.lang.sel(
                "The confirmation text did not match the profile name — nothing was deleted.",
                "확인 입력이 프로파일 이름과 다릅니다 — 아무것도 삭제하지 않았습니다.",
            ),
            Some(ctx.lang.sel(
                "Type the name exactly as shown.",
                "표시된 이름을 그대로 입력하세요.",
            )),
        );
        return (
            StatusCode::BAD_REQUEST,
            shell(
                &ctx,
                view::delete_confirm_body(ctx.lang, &schedule, Utc::now(), Some(notice)),
            ),
        )
            .into_response();
    }

    let args = vec![
        format!("expr={}", schedule.expr.as_str()),
        format!("type={}", schedule.kind.token()),
    ];
    let args: Vec<String> = args
        .into_iter()
        .map(|arg| request_registry(&ctx).mask(&arg))
        .collect();
    let receipt = match gate(&ctx, ACTION_DELETE, &schedule.profile, &args).await {
        Ok(receipt) => receipt,
        Err(problem) => return internal_problem(&ctx, &problem),
    };

    match store.delete(id, receipt).await {
        Ok(removed) => {
            record_outcome(&ctx, ACTION_DELETE, &removed.profile, &args, true).await;
            let notice = view::deleted_notice(ctx.lang, &removed.profile);
            render_list(&ctx, Some(notice)).await.into_response()
        }
        Err(e) => {
            record_outcome(&ctx, ACTION_DELETE, &schedule.profile, &args, false).await;
            internal_problem(&ctx, &Problem::internal(e.to_string()))
        }
    }
}

// ---------------------------------------------------------------------------
// 렌더 헬퍼
// ---------------------------------------------------------------------------

/// 목록 화면을 만든다 — crontab 조회를 포함한다(모듈 헤더 "crontab 조회를 화면 로드마다").
async fn render_list(ctx: &Arc<ServeConfig>, notice: Option<Markup>) -> Markup {
    let store = schedules::shared(&ctx.state_dir);
    let crontab = schedule::detect_crontab(ctx.jobs.secret_registry()).await;
    shell(
        ctx,
        view::list_body(
            ctx.lang,
            &store.list(),
            &store.report(),
            &crontab,
            Utc::now(),
            notice,
        ),
    )
}

/// 폼 화면을 만든다.
async fn render_form(
    ctx: &Arc<ServeConfig>,
    mode: FormMode,
    values: &FormValues,
    preview: Option<chrono::DateTime<Utc>>,
) -> Markup {
    // 등록 화면에서도 crontab 충돌을 경고한다(t30 요구사항 2번) — 스케줄을 **만드는
    // 순간**이 "cron에 같은 것이 있다"를 알려야 하는 가장 중요한 시점이다.
    let crontab = schedule::detect_crontab(ctx.jobs.secret_registry()).await;
    let notice = if crontab.conflicts() {
        Some(view::crontab_notice(ctx.lang, &crontab))
    } else {
        None
    };
    shell(
        ctx,
        view::form_body(
            ctx.lang,
            mode,
            values,
            &load_profile_names(ctx),
            preview,
            notice,
        ),
    )
}

/// 폼을 문제 알림과 함께 되돌린다(400).
async fn form_problem(
    ctx: &Arc<ServeConfig>,
    mode: FormMode,
    values: &FormValues,
    problem: &Problem,
) -> Response {
    let crontab = schedule::detect_crontab(ctx.jobs.secret_registry()).await;
    let mut notice = view::problem_notice(ctx.lang, &problem.message, problem.hint.as_deref());
    if crontab.conflicts() {
        // 두 알림을 한 블록으로 잇는다 — 문제 알림이 crontab 경고를 밀어내지 않게.
        notice = maud::html! {
            (notice)
            (view::crontab_notice(ctx.lang, &crontab))
        };
    }
    (
        problem.status,
        shell(
            ctx,
            view::form_body(
                ctx.lang,
                mode,
                values,
                &load_profile_names(ctx),
                None,
                Some(notice),
            ),
        ),
    )
        .into_response()
}

/// 서버 쪽 문제(500) — 목록 위에 알림으로 얹는다.
fn internal_problem(ctx: &Arc<ServeConfig>, problem: &Problem) -> Response {
    let store = schedules::shared(&ctx.state_dir);
    let notice = view::problem_notice(ctx.lang, &problem.message, problem.hint.as_deref());
    (
        problem.status,
        shell(
            ctx,
            view::list_body(
                ctx.lang,
                &store.list(),
                &store.report(),
                &schedule::CrontabScan::default(),
                Utc::now(),
                Some(notice),
            ),
        ),
    )
        .into_response()
}

/// 잘못된 id(400).
fn malformed(ctx: &Arc<ServeConfig>) -> Response {
    (
        StatusCode::BAD_REQUEST,
        shell(ctx, view::malformed_id(ctx.lang)),
    )
        .into_response()
}

/// 경로 파라미터가 잘못됐거나 없는 스케줄이다 — 400과 404를 구분한다.
///
/// 구분하는 이유는 [`crate::web::routes::jobs`]와 같다: 앞은 링크가 잘못된 것이고, 뒤는 다른
/// 탭에서 삭제됐거나 다른 state 디렉터리를 의심할 일이다.
fn missing(ctx: &Arc<ServeConfig>, raw: &str) -> Response {
    if ScheduleId::parse(raw).is_err() {
        return malformed(ctx);
    }
    (StatusCode::NOT_FOUND, shell(ctx, view::not_found(ctx.lang))).into_response()
}

/// 껍데기로 감싼다.
fn shell(ctx: &Arc<ServeConfig>, body: Markup) -> Markup {
    layout::shell(ctx.lang, SCHEDULE_TITLE, body)
}

/// 경로 파라미터를 스케줄로 접는다.
fn resolve(ctx: &Arc<ServeConfig>, raw: &str) -> Option<Schedule> {
    let id = ScheduleId::parse(raw).ok()?;
    schedules::shared(&ctx.state_dir).get(id)
}

// ---------------------------------------------------------------------------
// 검증
// ---------------------------------------------------------------------------

/// 사용자에게 보여줄 문제 하나.
struct Problem {
    status: StatusCode,
    message: String,
    hint: Option<String>,
}

impl Problem {
    fn bad_request(message: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
            hint: None,
        }
    }

    fn with_hint(message: String, hint: String) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message,
            hint: Some(hint),
        }
    }

    fn internal(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
            hint: None,
        }
    }

    /// 서버 쪽 문제 + 조치 안내. 감사 로그를 쓸 수 없는 경우가 여기다 — 사용자가 폼 값을
    /// 고쳐서 해결할 수 있는 문제가 아니므로 400이 아니라 500이어야 한다.
    fn internal_with_hint(message: String, hint: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message,
            hint: Some(hint),
        }
    }
}

/// 폼을 검증된 초안으로 접는다.
///
/// ## 앞으로 실행되지 않는 표현식은 저장 단계에서 거부한다
/// `0 0 30 2 *`(2월 30일)은 문법적으로 유효하지만 영원히 맞지 않는다([`CronExpr::next_after`]가
/// `None`을 돌려준다). 그런 스케줄을 저장하면 목록에서 활성으로 보이면서 아무 일도 하지
/// 않는다 — 이 기능에서 가장 나쁜 실패("돌고 있다고 믿는데 안 돈다")의 정확한 형태다. 그래서
/// **저장 시점에** 막는다. 파서가 조합의 실현 가능성까지 검사하지 않는 것과 짝을 이룬다
/// (파서는 문법만 보고, 실현 가능성은 계산 한 곳에서 균일하게 판정한다).
fn build_draft(form: &FormBody, lang: Lang) -> std::result::Result<ScheduleDraft, Problem> {
    let profile = ProfileName::parse(form.get(FIELD_PROFILE).unwrap_or_default(), lang)
        .map_err(|e| Problem::bad_request(e.detail()))?;
    let expr = CronExpr::parse(form.get(FIELD_EXPR).unwrap_or_default(), lang)
        .map_err(|e| Problem::bad_request(e.detail()))?;
    let kind = ScheduleKind::parse(form.get(FIELD_KIND).unwrap_or("full"))
        .map_err(|e| Problem::bad_request(e.detail()))?;

    if expr.next_after(Utc::now()).is_none() {
        return Err(Problem::with_hint(
            format!(
                "cron 표현식 '{}'은 앞으로 맞는 시각이 없습니다 — 이 스케줄은 저장해도 \
                 영원히 실행되지 않습니다.",
                expr.as_str()
            ),
            "존재하지 않는 날짜 조합(예: `0 0 30 2 *` = 2월 30일)인지 확인하세요.".to_string(),
        ));
    }

    Ok(ScheduleDraft {
        profile,
        expr,
        kind,
        enabled: form.flag(FIELD_ENABLED),
    })
}

/// config에서 프로파일 이름을 읽는다.
///
/// [`crate::web::routes::backup`]에 같은 판단의 비공개 함수가 있다(재사용하지 않은 이유는
/// 그것이 비공개이고 그 파일이 이 태스크의 소유가 아니라는 것뿐이다). t27의
/// `ConfigDocument`를 쓰지 않는 이유도 그쪽과 같다 — 출처(provenance) 추적까지 계산하는데
/// 이 화면은 이름 목록만 필요하다.
fn load_profile_names(ctx: &ServeConfig) -> Vec<String> {
    let Some(path) = &ctx.config_path else {
        return Vec::new();
    };
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(config) = crate::config::file::Config::from_toml_str(&text) else {
        return Vec::new();
    };
    config.profiles.keys().cloned().collect()
}

// ---------------------------------------------------------------------------
// 감사
// ---------------------------------------------------------------------------

/// 이 요청에서 쓸 시크릿 레지스트리(러너가 아는 시크릿 + 세션 토큰) —
/// `routes::jobs::request_registry`와 같은 판단이다.
fn request_registry(ctx: &ServeConfig) -> SecretRegistry {
    let mut registry = ctx.jobs.secret_registry().clone();
    crate::web::mask::register_from_env_names(&mut registry, [crate::web::auth::ENV_WEB_TOKEN]);
    registry
}

/// 감사 로그에 남길 인자 — **마스킹은 이 함수의 책임이다**([`crate::web::audit`] 헤더:
/// 감사 모듈은 받은 문자열을 들여다보지 않는다).
///
/// 이 화면은 시크릿 값을 다루지 않는다(프로파일 이름·cron 표현식·종류뿐이다). 그래도
/// 레지스트리를 통과시키는 이유: 운영자가 표현식 칸에 무엇을 붙여넣었는지 우리가 통제하지
/// 못하고, 감사 로그는 append-only라 한 번 새면 지울 수 없다.
fn audit_args(draft: &ScheduleDraft, registry: &SecretRegistry) -> Vec<String> {
    [
        format!("expr={}", draft.expr.as_str()),
        format!("type={}", draft.kind.token()),
        format!("enabled={}", draft.enabled),
    ]
    .into_iter()
    .map(|arg| registry.mask(&arg))
    .collect()
}

/// 감사 게이트를 통과해 receipt를 얻는다. 실패하면 변경을 하지 않는다(모듈 헤더).
async fn gate(
    ctx: &Arc<ServeConfig>,
    action: &str,
    target: &str,
    args: &[String],
) -> std::result::Result<AuditReceipt, Problem> {
    ctx.audit
        .gate(AUDIT_ACTOR, action, target, args)
        .await
        .map_err(|e| {
            Problem::internal_with_hint(
                format!("감사 로그를 기록할 수 없어 스케줄을 변경하지 않았습니다: {e}"),
                "state 디렉터리의 디스크 여유·권한을 확인하세요. 기록되지 않은 스케줄 변경은 \
                 나중에 백업이 멈춘 원인을 찾을 수 없게 만들므로 거부합니다."
                    .to_string(),
            )
        })
}

/// 변경 결과를 감사 로그에 한 줄 더 남긴다(게이트의 `requested`와 짝을 이룬다).
///
/// 실패는 경고만 남긴다 — 여기서 실패를 전파해도 호출부가 할 수 있는 일이 없다(변경은 이미
/// 디스크에 있다). [`crate::web::routes::backup::spawn_tracked_job`]의 완료 기록과 같은 판단이다.
async fn record_outcome(
    ctx: &Arc<ServeConfig>,
    action: &str,
    target: &str,
    args: &[String],
    success: bool,
) {
    use crate::web::audit::{AuditEvent, AuditOutcome};
    if let Err(e) = ctx
        .audit
        .record(AuditEvent {
            actor: AUDIT_ACTOR,
            action,
            target,
            args_masked: args,
            outcome: if success {
                AuditOutcome::Success
            } else {
                AuditOutcome::Failure
            },
            exit_code: None,
        })
        .await
    {
        tracing::warn!(action, error = %e, "스케줄 변경 결과 감사 기록 실패");
    }
}

/// 출처가 다른 상태 변경 요청을 거부한다.
///
/// 라우터의 `route_layer(require_same_origin)`가 이미 같은 검사를 한다([`crate::web::server`]
/// 헤더). 그래도 핸들러에서 한 번 더 보는 이유는 [`crate::web::routes::config`]와 같다 —
/// 이 POST가 그 계층 **밖에** 배선되는 사고 하나로 CSRF 표면이 열리고, 그 사고는 라우터
/// 조립 코드를 읽어야만 보인다. 검사 비용은 헤더 두 개 비교다.
fn reject_foreign_origin(ctx: &ServeConfig, headers: &HeaderMap) -> Option<Response> {
    let refusal = crate::web::auth::verify_same_origin(headers).err()?;
    tracing::warn!(reason = ?refusal, "출처를 확인할 수 없어 스케줄 변경 요청을 거부했습니다");
    let notice = view::problem_notice(
        ctx.lang,
        &refusal.explain(ctx.lang),
        Some(ctx.lang.sel(
            "Submit the form from the console's own page. Nothing was changed.",
            "콘솔 화면에서 폼을 제출하세요. 아무것도 바뀌지 않았습니다.",
        )),
    );
    Some(
        (
            StatusCode::FORBIDDEN,
            layout::shell(ctx.lang, SCHEDULE_TITLE, notice),
        )
            .into_response(),
    )
}

// ---------------------------------------------------------------------------
// 폼 본문 파싱
// ---------------------------------------------------------------------------

/// `application/x-www-form-urlencoded` 최소 파서.
///
/// [`crate::web::routes::config`]·[`crate::web::routes::backup`]도 각자 같은 것을 갖고 있다
/// (둘 다 비공개다). 여섯 필드짜리 최소 버전으로 충분하고, 병행 작업 중인 파일에 결합을
/// 만들지 않는다 — 공용화는 후속 정리 대상이다.
struct FormBody {
    pairs: Vec<(String, String)>,
}

impl FormBody {
    fn parse(body: &str) -> Self {
        let mut pairs = Vec::new();
        for pair in body.as_bytes().split(|&b| b == b'&') {
            if pair.is_empty() {
                continue;
            }
            let (name, value) = match pair.iter().position(|&b| b == b'=') {
                Some(eq) => {
                    let (k, v) = pair.split_at(eq);
                    (percent_decode(k), percent_decode(&v[1..]))
                }
                None => (percent_decode(pair), String::new()),
            };
            pairs.push((name, value));
        }
        Self { pairs }
    }

    /// 값(앞뒤 공백 제거). 표현식·프로파일명은 셸/복사 붙여넣기에서 공백이 흔히 붙는다.
    fn get(&self, name: &str) -> Option<&str> {
        self.get_raw(name).map(str::trim)
    }

    /// 값(원본). **삭제 확인 비교는 이것을 쓴다** — 공백을 관대하게 접으면 확인의 의미가
    /// 약해진다.
    fn get_raw(&self, name: &str) -> Option<&str> {
        self.pairs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    fn flag(&self, name: &str) -> bool {
        self.get(name).is_some_and(|v| !v.is_empty())
    }
}

fn percent_decode(input: &[u8]) -> String {
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        match input[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < input.len()
                && input[i + 1].is_ascii_hexdigit()
                && input[i + 2].is_ascii_hexdigit() =>
            {
                out.push((hex_val(input[i + 1]) << 4) | hex_val(input[i + 2]));
                i += 3;
            }
            byte => {
                out.push(byte);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        b'A'..=b'F' => byte - b'A' + 10,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::web::auth::{self, AuthState};
    use axum::body::Body;
    use axum::http::{header, Request};
    use axum::middleware;
    use axum::routing::{get, post};
    use axum::Router;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    /// 테스트 세션 토큰.
    const TEST_TOKEN: &str = "test-token-schedule-4c7e";

    /// 이 모듈의 라우트 테스트가 공유하는 인증 상태와 **그 상태에서 유효한 세션 ID**.
    ///
    /// 세션 도입 뒤로 "인증을 통과한 요청"은 토큰을 쿠키에 넣어서 만들 수 없다 — 그게 정확히
    /// 고친 취약점이다(`crate::web::auth` 모듈 헤더 "쿠키 값은 토큰이 아니다"). 세션은 발급한
    /// 인스턴스에만 있으므로 미들웨어와 쿠키가 **같은** `AuthState`를 봐야 하고, 둘을 하나로
    /// 묶어 두면 그 실수를 할 수 없다.
    fn auth_and_session() -> &'static (Arc<AuthState>, String) {
        static PAIR: std::sync::OnceLock<(Arc<AuthState>, String)> = std::sync::OnceLock::new();
        PAIR.get_or_init(|| AuthState::for_test_with_session(TEST_TOKEN))
    }

    /// 유효한 세션 쿠키 값 — 인증을 통과해야 하는 요청이 싣는다.
    fn session() -> &'static str {
        &auth_and_session().1
    }

    /// 출처 검사를 통과하는 테스트 호스트.
    const TEST_HOST: &str = "127.0.0.1:8787";

    /// 이 화면들만 담은 최소 라우터.
    ///
    /// `server.rs`의 라우터를 쓰지 않는 이유: 이 태스크는 그 파일을 건드리지 않는다(배선은
    /// 리더의 몫). 그래서 **리더가 붙일 모양 그대로** 여기서 조립해, 배선되면 401/200과
    /// 출처 검사가 실제로 그렇게 동작한다는 것을 미리 고정한다 — `route_layer` 두 겹까지
    /// 같은 형태다([`crate::web::server`] 헤더의 근거).
    fn router(ctx: Arc<ServeConfig>) -> Router {
        Router::new()
            .route(SCHEDULE_PATH, get(list))
            .route(NEW_PATH, get(new_form))
            .route(EDIT_ROUTE, get(edit_form))
            .route(DELETE_CONFIRM_ROUTE, get(delete_form))
            .route(SAVE_PATH, post(save))
            .route(DELETE_PATH, post(delete_submit))
            .route_layer(middleware::from_fn(auth::require_same_origin))
            .route_layer(middleware::from_fn_with_state(
                Arc::clone(&auth_and_session().0),
                auth::require_auth,
            ))
            .with_state(ctx)
    }

    /// GET 하나. `cookie`가 `Some`이면 세션 쿠키를 싣는다.
    async fn get_page(
        ctx: Arc<ServeConfig>,
        uri: &str,
        cookie: Option<&str>,
    ) -> (StatusCode, String) {
        let mut builder = Request::builder().uri(uri).header(header::HOST, TEST_HOST);
        if let Some(token) = cookie {
            builder = builder.header(
                header::COOKIE,
                format!("{}={token}", auth::SESSION_COOKIE_NAME),
            );
        }
        send(ctx, builder.body(Body::empty()).unwrap()).await
    }

    /// POST 하나(폼 본문). 기본으로 같은 출처 헤더를 싣는다.
    async fn post_form(ctx: Arc<ServeConfig>, uri: &str, body: &str) -> (StatusCode, String) {
        let request = Request::builder()
            .method("POST")
            .uri(uri)
            .header(header::HOST, TEST_HOST)
            .header(header::ORIGIN, format!("http://{TEST_HOST}"))
            .header(header::CONTENT_TYPE, "application/x-www-form-urlencoded")
            .header(
                header::COOKIE,
                format!("{}={}", auth::SESSION_COOKIE_NAME, session()),
            )
            .body(Body::from(body.to_string()))
            .unwrap();
        send(ctx, request).await
    }

    async fn send(ctx: Arc<ServeConfig>, request: Request<Body>) -> (StatusCode, String) {
        let response = router(ctx)
            .oneshot(request)
            .await
            .expect("라우터 호출 실패");
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, String::from_utf8_lossy(&bytes).into_owned())
    }

    /// 스케줄 하나를 만든 컨텍스트.
    async fn ctx_with_one_schedule() -> (Arc<ServeConfig>, ScheduleId) {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            "profile=prod&expr=0+3+*+*+*&kind=full&enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::OK, "생성 실패: {body}");
        let id = schedules::shared(&ctx.state_dir).list()[0].id;
        (ctx, id)
    }

    /// 모든 화면이 쿠키 없이는 401이고 내용을 흘리지 않는다.
    #[tokio::test]
    async fn every_screen_requires_auth() {
        let (ctx, id) = ctx_with_one_schedule().await;
        for uri in [
            SCHEDULE_PATH.to_string(),
            NEW_PATH.to_string(),
            edit_href(id),
            delete_confirm_href(id),
        ] {
            let (status, body) = get_page(Arc::clone(&ctx), &uri, None).await;
            assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri}가 열려 있다");
            assert!(!body.contains(SCHEDULE_TITLE), "{uri}가 내용을 흘렸다");
            assert!(!body.contains("prod"), "{uri}가 프로파일명을 흘렸다");
        }
    }

    /// 쿠키가 있으면 200이고 스케줄이 보인다.
    #[tokio::test]
    async fn cookie_opens_the_screens() {
        let (ctx, id) = ctx_with_one_schedule().await;
        for uri in [
            SCHEDULE_PATH.to_string(),
            NEW_PATH.to_string(),
            edit_href(id),
            delete_confirm_href(id),
        ] {
            let (status, body) = get_page(Arc::clone(&ctx), &uri, Some(session())).await;
            assert_eq!(status, StatusCode::OK, "{uri}: {body}");
            assert!(body.contains(SCHEDULE_TITLE), "{uri}에 제목이 없다");
        }
        let (_, body) = get_page(Arc::clone(&ctx), SCHEDULE_PATH, Some(session())).await;
        assert!(body.contains("0 3 * * *"), "표현식이 보이지 않는다: {body}");
        assert!(body.contains("Cron (UTC)"), "기준 표기가 없다");
    }

    /// 어떤 화면에도 세션 토큰이 새지 않는다.
    #[tokio::test]
    async fn no_screen_leaks_the_session_token() {
        let (ctx, id) = ctx_with_one_schedule().await;
        for uri in [
            SCHEDULE_PATH.to_string(),
            NEW_PATH.to_string(),
            edit_href(id),
            delete_confirm_href(id),
        ] {
            let (_, body) = get_page(Arc::clone(&ctx), &uri, Some(session())).await;
            assert!(!body.contains(TEST_TOKEN), "{uri}가 세션 토큰을 노출했다");
        }
    }

    /// 생성 → 수정 → 삭제가 화면으로 왕복한다.
    #[tokio::test]
    async fn crud_round_trips_through_the_screens() {
        let (ctx, id) = ctx_with_one_schedule().await;

        // 수정.
        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            &format!("id={id}&profile=prod&expr=%2A%2F30+*+*+*+*&kind=incr&enabled=1"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("Schedule saved"), "성공 알림이 없다: {body}");
        let stored = schedules::shared(&ctx.state_dir).list();
        assert_eq!(stored[0].expr.as_str(), "*/30 * * * *");
        assert_eq!(stored[0].kind, ScheduleKind::Incr);

        // 삭제 — 확인 입력이 맞아야 한다.
        let (status, body) = post_form(
            Arc::clone(&ctx),
            DELETE_PATH,
            &format!("id={id}&confirm=wrong"),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert!(
            !schedules::shared(&ctx.state_dir).list().is_empty(),
            "확인이 틀렸는데 삭제됐다"
        );

        let (status, body) = post_form(
            Arc::clone(&ctx),
            DELETE_PATH,
            &format!("id={id}&confirm=prod"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(
            body.contains("Schedule deleted"),
            "삭제 알림이 없다: {body}"
        );
        assert!(schedules::shared(&ctx.state_dir).list().is_empty());
    }

    /// 잘못된 표현식은 400으로 되돌아오고 **적은 값이 폼에 남는다**.
    #[tokio::test]
    async fn a_bad_expression_returns_400_and_keeps_the_input() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            "profile=prod&expr=60+*+*+*+*&kind=full&enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // 테스트 컨텍스트는 En이다 — 화면 언어에 맞는 문장이 나와야 한다.
        assert!(
            body.contains("only 0-59 are allowed"),
            "구체적 이유가 없다: {body}"
        );
        assert!(
            body.contains(r#"value="60 * * * *""#),
            "입력이 사라졌다: {body}"
        );
        assert!(schedules::shared(&ctx.state_dir).list().is_empty());
    }

    /// **앞으로 실행되지 않는 표현식**은 저장 단계에서 거부된다 — 활성으로 보이면서 아무
    /// 일도 하지 않는 스케줄을 만들지 않는다([`build_draft`] doc).
    #[tokio::test]
    async fn an_expression_that_never_matches_is_refused_at_save() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            // 2월 30일.
            "profile=prod&expr=0+0+30+2+*&kind=full&enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(
            body.contains("앞으로 맞는 시각이 없습니다") || body.contains("never"),
            "{body}"
        );
        assert!(schedules::shared(&ctx.state_dir).list().is_empty());
    }

    /// 중복 등록은 400으로 거부된다(새로고침 재제출이 두 개를 만들지 않는다 — 모듈 헤더).
    #[tokio::test]
    async fn resubmitting_a_create_does_not_make_two() {
        let (ctx, _) = ctx_with_one_schedule().await;
        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            "profile=prod&expr=0+3+*+*+*&kind=full&enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(schedules::shared(&ctx.state_dir).list().len(), 1);
    }

    /// 잘못된 id는 400, 없는 id는 404다.
    #[tokio::test]
    async fn bad_and_missing_ids_are_distinguished() {
        let ctx = Arc::new(ServeConfig::for_test());
        let (status, _) =
            get_page(Arc::clone(&ctx), "/schedule/edit/..%2Fetc", Some(session())).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "경로 조작이 400이 아니다");
        let (status, _) = get_page(
            Arc::clone(&ctx),
            &edit_href(ScheduleId::generate()),
            Some(session()),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// 다른 출처의 POST는 403으로 거부되고 아무것도 바뀌지 않는다.
    #[tokio::test]
    async fn cross_origin_posts_change_nothing() {
        let ctx = Arc::new(ServeConfig::for_test());
        let request = Request::builder()
            .method("POST")
            .uri(SAVE_PATH)
            .header(header::HOST, TEST_HOST)
            .header(header::ORIGIN, "http://evil.example")
            .header(
                header::COOKIE,
                format!("{}={}", auth::SESSION_COOKIE_NAME, session()),
            )
            .body(Body::from(
                "profile=prod&expr=0+3+*+*+*&kind=full&enabled=1",
            ))
            .unwrap();
        let (status, _) = send(Arc::clone(&ctx), request).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert!(schedules::shared(&ctx.state_dir).list().is_empty());
    }

    /// 스케줄 변경이 **감사 로그에 남는다** — 요청 줄과 결과 줄 두 개.
    #[tokio::test]
    async fn schedule_changes_are_audited() {
        let (ctx, id) = ctx_with_one_schedule().await;
        post_form(
            Arc::clone(&ctx),
            DELETE_PATH,
            &format!("id={id}&confirm=prod"),
        )
        .await;

        let text = std::fs::read_to_string(ctx.audit.path()).expect("감사 로그 읽기 실패");
        let lines: Vec<serde_json::Value> = text
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).expect("감사 로그가 NDJSON이 아니다"))
            .collect();
        let actions: Vec<&str> = lines
            .iter()
            .filter_map(|l| l.get("action").and_then(|v| v.as_str()))
            .collect();
        assert!(
            actions.contains(&ACTION_CREATE),
            "생성 기록이 없다: {actions:?}"
        );
        assert!(
            actions.contains(&ACTION_DELETE),
            "삭제 기록이 없다: {actions:?}"
        );
        // 게이트(requested) + 결과(success)가 짝을 이룬다.
        let outcomes: Vec<&str> = lines
            .iter()
            .filter(|l| l.get("action").and_then(|v| v.as_str()) == Some(ACTION_CREATE))
            .filter_map(|l| l.get("outcome").and_then(|v| v.as_str()))
            .collect();
        assert!(
            outcomes.contains(&"requested"),
            "게이트 줄이 없다: {outcomes:?}"
        );
        assert!(
            outcomes.contains(&"success"),
            "결과 줄이 없다: {outcomes:?}"
        );
        // actor는 사람이 누른 것이므로 "web"이다 — 스케줄러 자동 실행과 구분된다.
        for line in &lines {
            assert_eq!(
                line.get("actor").and_then(|v| v.as_str()),
                Some(AUDIT_ACTOR),
                "화면에서 온 변경의 actor가 web이 아니다"
            );
        }
        assert_ne!(
            AUDIT_ACTOR,
            schedule::SCHEDULER_ACTOR,
            "사람과 스케줄러의 actor가 같으면 감사에서 구분할 수 없다"
        );
    }

    /// 감사 로그를 쓸 수 없으면 스케줄이 바뀌지 않는다(fail-closed — 모듈 헤더).
    #[tokio::test]
    #[cfg(unix)]
    async fn an_unwritable_audit_log_blocks_the_change() {
        use std::os::unix::fs::PermissionsExt;
        let ctx = Arc::new(ServeConfig::for_test());
        // 감사 로그 파일을 읽기 전용으로 만든다 — append가 실패한다.
        let path = ctx.audit.path().to_path_buf();
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();

        let (status, body) = post_form(
            Arc::clone(&ctx),
            SAVE_PATH,
            "profile=prod&expr=0+3+*+*+*&kind=full&enabled=1",
        )
        .await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
        assert!(body.contains("감사 로그"), "이유를 말하지 않는다: {body}");
        assert!(
            schedules::shared(&ctx.state_dir).list().is_empty(),
            "감사 기록 없이 스케줄이 저장됐다"
        );

        // 정리 — 임시 디렉터리 제거가 막히지 않게 권한을 돌려준다.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }

    /// 손상된 스케줄 파일에서도 화면이 200으로 뜨고 **크게 경고한다**(fail-loud).
    #[tokio::test]
    async fn a_corrupt_state_file_still_renders_with_a_loud_banner() {
        let ctx = Arc::new(ServeConfig::for_test());
        std::fs::write(
            ctx.state_dir.join(schedules::SCHEDULES_FILE_NAME),
            b"{ truncated",
        )
        .unwrap();
        let (status, body) = get_page(Arc::clone(&ctx), SCHEDULE_PATH, Some(session())).await;
        assert_eq!(status, StatusCode::OK, "손상이 화면을 막았다");
        assert!(
            body.contains("could not be read") || body.contains("읽지 못했습니다"),
            "손상 경고가 없다: {body}"
        );
        assert!(
            body.contains(r#"data-level="error""#),
            "경고 레벨이 실려 있지 않다"
        );
    }

    /// 폼 파서는 퍼센트 인코딩·`+`·원본 보존을 정확히 다룬다.
    #[test]
    fn form_parsing_handles_encoding_and_preserves_raw() {
        let form = FormBody::parse("expr=%2A%2F15+*+*+*+*&confirm=+prod+&kind=full");
        assert_eq!(form.get(FIELD_EXPR), Some("*/15 * * * *"));
        // 확인 비교는 원본을 쓴다 — 공백이 붙은 입력은 일치하지 않아야 한다.
        assert_eq!(form.get_raw(FIELD_CONFIRM), Some(" prod "));
        assert_eq!(form.get(FIELD_CONFIRM), Some("prod"));
        assert!(!form.flag(FIELD_ENABLED), "없는 체크박스가 참이다");
        assert!(FormBody::parse("enabled=1").flag(FIELD_ENABLED));
    }

    /// 경로 상수와 링크 헬퍼가 어긋나지 않는다 — 패턴을 href에 그대로 쓰면 `{id}`가 URL에
    /// 남는다.
    #[test]
    fn route_patterns_and_hrefs_agree() {
        let id = ScheduleId::generate();
        assert_eq!(edit_href(id), format!("/schedule/edit/{id}"));
        assert_eq!(delete_confirm_href(id), format!("/schedule/delete/{id}"));
        assert!(!edit_href(id).contains('{'), "패턴이 링크에 새어 나왔다");
        assert_eq!(EDIT_ROUTE, "/schedule/edit/{id}");
        assert_eq!(DELETE_CONFIRM_ROUTE, "/schedule/delete/{id}");
        // 삭제 확인(GET)과 삭제 제출(POST)은 다른 경로다 — 같으면 axum이 한 라우트에
        // 두 메서드를 붙여야 하고, 확인 없는 삭제 링크가 만들어질 수 있다.
        assert_ne!(DELETE_PATH, DELETE_CONFIRM_ROUTE);
    }
}
