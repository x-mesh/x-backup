//! `status` 서브커맨드 핸들러 — 대상 서버 상태를 읽기 전용으로 점검(FR-8, R14).
//!
//! config 로드 → URI 해석 → [`StatusChecker`] 전체 점검 → 신호등 요약 출력(사람/`--json`).
//! 종료 코드는 신호등 합산으로 결정한다(PRD §9): fail=3(사전 점검 실패), warn=4(경고
//! 동반 성공), 모두 ok면 0.
//!
//! 무부작용·읽기 전용이다 — 어떤 쓰기/변경도 하지 않는다(PRD §FR-8 동작 요건).

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::time::Duration;

use crate::cli::args::StatusArgs;
use crate::cli::table::{
    display_width, pad, pad_left, paint, use_color, Align, BOLD, CYAN, DIM, GREEN, RED, RESET,
    UNDERLINE, YELLOW,
};
use crate::config::env::collect_overrides_from_process;
use crate::config::file::Profile;
use crate::config::merged::MergeInput;
use crate::config::secret::Secret;
use crate::config::ResolvedConfig;
use crate::engine::mongo::status::{human_bytes, CheckStatus, StatusChecker, StatusReport};
use crate::engine::mongo::MongoMeta;
use crate::error::{Result, XBackupError};
use crate::i18n::Lang;
use crate::manifest::schema::{BackupManifest, BackupType};
use crate::manifest::ManifestStore;
use crate::storage::{from_config, BoxAsyncRead, Storage};

/// backup 자동 사전 점검에서 쓰는 mongodump 실행파일 이름(backup 핸들러와 동일 기본값).
pub const DEFAULT_MONGODUMP: &str = "mongodump";

/// `status` 핸들러 진입점.
pub async fn handle(
    config_path: Option<PathBuf>,
    lang_flag: Option<crate::i18n::Lang>,
    args: StatusArgs,
) -> Result<()> {
    // config 파일은 한 번만 읽는다(--all은 여러 프로파일에 재사용).
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let lang = crate::i18n::resolve_from_toml(lang_flag, config_toml.as_deref());

    // 참조 중인 config 위치를 stderr에 한 줄 알린다(다중 DB 툴 + xbenv 자동 XB_CONFIG 환경에서
    // "지금 어느 config를 보는지"를 분명히). json은 기계 판독 오염 방지로 생략.
    if !args.json {
        let src = crate::cli::output::config_source_label(config_path.as_deref(), lang);
        eprintln!(
            "{}",
            paint(&format!("▸ config: {src}"), &[DIM], use_color())
        );
    }

    // 라이브 모드 — 주기 갱신하며 변경량(Δ)을 추적한다(단일/--all 모두 지원).
    if args.watch {
        return handle_watch(config_toml.as_deref(), &args, lang).await;
    }

    if args.all {
        return handle_all(config_toml.as_deref(), args.json, args.ns_detail, lang).await;
    }

    // 단일 프로파일 — 미지정이면 빈 문자열로 두어 build가 config의 default_profile로
    // 폴백한다(list/backup과 일관). 셋 다 없으면 build/ResolvedConfig가 명확히 거부한다.
    let profile = args.profile.as_deref().unwrap_or("");
    // --json은 기계 판독 안정성을 위해 언어를 En으로 고정한다(사람 출력만 resolve된 lang 사용).
    let report_lang = if args.json {
        crate::i18n::Lang::En
    } else {
        lang
    };
    let report = build_report(config_toml.as_deref(), profile, report_lang).await?;

    // 실행 컨텍스트(프로파일·DB) — 표시용으로 source URI 엔진을 가볍게 재해석(연결 없음).
    // profile이 빈 문자열(XB_PROFILE="" 잔재)이면 build가 default_profile로 폴백하므로,
    // 표시도 원시 빈 값이 아닌 실효 프로파일 이름(resolved.profile_name)을 쓴다.
    let resolved_ctx = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: profile,
        overrides: &collect_overrides_from_process(),
    })
    .ok();
    let display_profile: String = resolved_ctx
        .as_ref()
        .map(|r| r.profile_name.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| profile.to_string());
    let db = resolved_ctx
        .and_then(|r| r.resolved_uri)
        .map(|u| crate::engine::DbKind::from_uri(u.expose()));
    crate::cli::output::print_run_context(
        &display_profile,
        db,
        crate::cli::output::context_mode(args.json),
    );

    // 출력 — --json 구조화 또는 사람용 표. --ns-detail이면 ns별 문서 수를 덧붙인다.
    if args.json {
        if args.ns_detail {
            let counts = collect_ns_counts(config_toml.as_deref(), profile)
                .await
                .unwrap_or_default();
            let mut v = serde_json::to_value(&report)
                .map_err(|e| XBackupError::Failure(format!("status JSON 직렬화 실패: {e}")))?;
            if let Some(obj) = v.as_object_mut() {
                obj.insert("namespaces".to_string(), ns_counts_json(&counts));
            }
            println!(
                "{}",
                serde_json::to_string_pretty(&v)
                    .map_err(|e| XBackupError::Failure(format!("status JSON 직렬화 실패: {e}")))?
            );
        } else {
            render_json(&report)?;
        }
    } else {
        render_human(&report, lang);
        if args.ns_detail {
            match collect_ns_counts(config_toml.as_deref(), profile).await {
                Ok(counts) => print_ns_detail_human(profile, &counts, lang),
                Err(e) => eprintln!("⚠ ns-detail 조회 실패(profile {profile}): {e}"),
            }
        }
    }

    // 신호등 → 종료 코드. fail이면 PrecheckFailed(exit 3), warn이면 Warning(exit 4), ok면 0.
    report_to_result(&report)
}

/// `--all` — config의 모든 프로파일을 점검한다. 사람용 출력은 **프로파일별 전체 상세**
/// (단일 `status`와 동일한 신호등 표)를 차례로 찍은 뒤, 마지막에 한 줄씩 종합 표를 붙인다.
/// 종료 코드는 최악 신호등을 따른다. `-v`는 로그 레벨일 뿐 이 보고서 상세도와 무관하다.
async fn handle_all(
    config_toml: Option<&str>,
    json: bool,
    ns_detail: bool,
    lang: Lang,
) -> Result<()> {
    let raw = config_toml.ok_or_else(|| {
        XBackupError::Usage("--all에는 config 파일이 필요합니다(프로파일 목록)".into())
    })?;
    let config = crate::config::file::Config::from_toml_str(raw)?;
    let mut names: Vec<String> = config.profiles.keys().cloned().collect();
    names.sort();
    if names.is_empty() {
        return Err(XBackupError::Usage(
            "config에 프로파일이 없습니다([profiles.<name>])".into(),
        ));
    }

    // --json은 기계 판독 안정성을 위해 언어를 En으로 고정한다.
    let report_lang = if json { crate::i18n::Lang::En } else { lang };
    let mut reports = Vec::with_capacity(names.len());
    for name in &names {
        reports.push(build_report(config_toml, name, report_lang).await?);
    }

    // --ns-detail이면 프로파일별 ns 카운트를 best-effort로 미리 모은다(names와 인덱스 정렬).
    // 연결 실패는 Err 문자열로 남겨 human은 경고로, json은 빈 배열로 처리한다.
    let ns_results: Vec<std::result::Result<Vec<(String, u64)>, String>> = if ns_detail {
        let mut v = Vec::with_capacity(names.len());
        for name in &names {
            v.push(
                collect_ns_counts(config_toml, name)
                    .await
                    .map_err(|e| e.to_string()),
            );
        }
        v
    } else {
        Vec::new()
    };

    // 출력.
    if json {
        let items: Vec<serde_json::Value> = reports
            .iter()
            .enumerate()
            .map(|(idx, r)| {
                let mut obj = serde_json::json!({
                    "profile": r.profile,
                    "overall": format!("{:?}", r.overall).to_lowercase(),
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "key": i.key,
                        "label": i.label,
                        "status": format!("{:?}", i.status).to_lowercase(),
                        "value": i.value,
                        "message": i.message,
                    })).collect::<Vec<_>>(),
                });
                if ns_detail {
                    let counts = ns_results[idx]
                        .as_ref()
                        .map(|v| v.as_slice())
                        .unwrap_or(&[]);
                    obj.as_object_mut()
                        .unwrap()
                        .insert("namespaces".to_string(), ns_counts_json(counts));
                }
                obj
            })
            .collect();
        println!("{}", serde_json::json!({ "profiles": items }));
    } else if reports.len() == 1 {
        // 프로파일이 하나면 비교 의미가 없다 — 단일 status 상세 표 그대로.
        render_human(&reports[0], lang);
    } else {
        // 둘 이상이면 source(왼쪽=첫 열) 기준 비교 표.
        render_comparison(&reports, lang);
    }

    // --ns-detail 사람용 — 비교 표 뒤에 프로파일별 ns 섹션을 붙인다(json은 위에서 끼웠음).
    if ns_detail && !json {
        for (idx, name) in names.iter().enumerate() {
            println!();
            match &ns_results[idx] {
                Ok(counts) => print_ns_detail_human(name, counts, lang),
                Err(e) => eprintln!("⚠ ns-detail 조회 실패(profile {name}): {e}"),
            }
        }
    }

    // 최악 신호등으로 종료 코드 결정.
    let worst = reports
        .iter()
        .map(|r| r.overall)
        .max_by_key(|s| match s {
            CheckStatus::Ok => 0,
            CheckStatus::Warn => 1,
            CheckStatus::Fail => 2,
        })
        .unwrap_or(CheckStatus::Ok);
    report_to_result(&StatusReport::new("(전체)", overall_placeholder(worst)))
}

/// 한 프로파일의 점검 보고서를 만든다(connect 실패도 보고서로 표현 — Err로 끊지 않음).
async fn build_report(
    config_toml: Option<&str>,
    profile: &str,
    lang: crate::i18n::Lang,
) -> Result<StatusReport> {
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml,
        profile_name: profile,
        overrides: &overrides,
    })?;
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri/uri_env가 없습니다",
            resolved.profile_name
        ))
    })?;
    let interval = resolved.profile.features.incremental.interval.clone();
    let prefer_secondary = resolved.profile.source.prefer_secondary;
    let timeout = resolved.profile.source.connect_timeout_secs;

    // DB 종류 분기 — postgres/mysql URI면 드라이버 status, 그 외는 Mongo status.
    let db_kind = crate::engine::DbKind::from_uri(uri.expose());
    let report = if db_kind == crate::engine::DbKind::Postgres {
        crate::engine::postgres::status::full_report(&resolved.profile_name, &uri, timeout, lang)
            .await
    } else if db_kind == crate::engine::DbKind::Mysql {
        crate::engine::mysql::status::full_report(&resolved.profile_name, &uri, timeout, lang).await
    } else {
        // 엔진별 도구 점검 — native는 외부 도구 불필요(None), mongodump는 도구 존재/버전 점검.
        let tool = match crate::pipeline::backup::Engine::parse(&resolved.profile.mode.engine)? {
            crate::pipeline::backup::Engine::Native => None,
            crate::pipeline::backup::Engine::Mongodump => Some(DEFAULT_MONGODUMP),
        };
        match StatusChecker::connect(&uri, timeout).await {
            Ok(checker) => {
                checker
                    .full_report(
                        &resolved.profile_name,
                        tool,
                        &interval,
                        prefer_secondary,
                        lang,
                    )
                    .await
            }
            Err(e) => StatusReport::new(
                &resolved.profile_name,
                vec![crate::engine::mongo::status::CheckItem::fail(
                    "connection",
                    "connection / auth",
                    lang.sel(
                        format!("connection setup failed: {e}").as_str(),
                        format!("연결 준비 실패: {e}").as_str(),
                    ),
                )],
            ),
        }
    };

    // destination 쪽 점검(source 연결과 무관) — 마지막 백업·destination 쓰기 가능 여부를
    // 보고서에 덧붙인다. 백업 도구로서 "내 백업이 최신/대상이 정상인가"를 같이 보여준다.
    let crate::engine::mongo::status::StatusReport {
        profile: name,
        mut items,
        ..
    } = report;
    if resolved.profile.is_endpoint_only() {
        // endpoint 전용(복구/이관 대상) — 백업 저장소가 없는 게 정상이므로 destination/
        // last-backup 점검은 "해당 없음"으로 표기한다(오탐 WARN 방지).
        use crate::engine::mongo::status::CheckItem;
        items.push(
            CheckItem::ok(
                "last_backup",
                "last backup",
                lang.sel(
                    "n/a — endpoint-only profile",
                    "해당 없음 — endpoint 전용 프로파일",
                ),
            )
            .with_value(lang.sel("n/a", "해당없음")),
        );
        items.push(
            CheckItem::ok(
                "recoverable",
                "PITR recoverable",
                lang.sel(
                    "n/a — endpoint-only profile",
                    "해당 없음 — endpoint 전용 프로파일",
                ),
            )
            .with_value(lang.sel("n/a", "해당없음")),
        );
        items.push(
            CheckItem::ok(
                "destination",
                "destination",
                lang.sel(
                    "endpoint only — restore/migrate target (no backup storage)",
                    "endpoint 전용 — 복구/이관 대상(백업 저장소 없음)",
                ),
            )
            .with_value(lang.sel("endpoint only", "endpoint 전용")),
        );
    } else {
        items.push(last_backup_item(&resolved.profile, lang).await);
        items.push(recoverable_item(&resolved.profile, lang).await);
        items.push(destination_item(&resolved.profile, lang).await);
    }
    Ok(StatusReport::new(name, items))
}

/// destination의 최신 manifest를 읽어 "마지막 백업" 항목을 만든다(나이·타입·크기). 무변경.
async fn last_backup_item(
    profile: &Profile,
    lang: crate::i18n::Lang,
) -> crate::engine::mongo::status::CheckItem {
    use crate::engine::mongo::status::CheckItem;
    let dests = profile.effective_destinations();
    let dest = match dests.first() {
        Some(d) => *d,
        None => {
            return CheckItem::warn(
                "last_backup",
                "last backup",
                lang.sel("destination not configured", "destination 미설정"),
            )
            .with_value(lang.sel("not configured", "미설정"))
        }
    };
    let storage = match from_config(dest) {
        Ok(s) => s,
        Err(e) => {
            return CheckItem::warn(
                "last_backup",
                "last backup",
                lang.sel(
                    format!("destination access failed: {e}").as_str(),
                    format!("destination 접근 실패: {e}").as_str(),
                ),
            )
            .with_value(lang.sel("access failed", "접근 실패"))
        }
    };
    match latest_manifest_any(storage.as_ref()).await {
        Some(m) => {
            let age = format_age_rfc3339(&m.created_at, lang);
            let typ = match m.backup_type {
                BackupType::Full => "full",
                BackupType::Incremental => "incr",
            };
            CheckItem::ok(
                "last_backup",
                "last backup",
                lang.sel(
                    format!(
                        "{typ}, {}, {age} ago (id {})",
                        human_bytes(m.stored_size_bytes as i64),
                        short_id(&m.id)
                    )
                    .as_str(),
                    format!(
                        "{typ}, {}, {age} 전 (id {})",
                        human_bytes(m.stored_size_bytes as i64),
                        short_id(&m.id)
                    )
                    .as_str(),
                ),
            )
            .with_value(lang.sel(format!("{age} ago").as_str(), format!("{age} 전").as_str()))
        }
        None => CheckItem::warn(
            "last_backup",
            "last backup",
            lang.sel("no backup history", "백업 이력이 없습니다"),
        )
        .with_value(lang.sel("none", "없음")),
    }
}

/// 카탈로그(manifest)만으로 **현재 복구 가능한 최신 시점**과 **RPO 갭**을 보고한다(PRD-03 P1).
///
/// - `recoverable_until` = 최신 백업의 실효 복구 시점: oplog_range가 있으면 그 `end_ts`(정밀),
///   없으면(PG/MySQL 풀 등) `created_at`으로 근사한다([`recoverable_until_rfc3339`]).
/// - `rpo_gap` = now − recoverable_until. 값 컬럼(`--json`의 items[key=recoverable].value)에는
///   RFC3339 시점을, 메시지에는 갭을 함께 담는다.
///
/// 서버 접속 없이 destination 카탈로그만 읽으므로 오프라인에서도 동작한다. base가 없으면
/// "복구 불가"로 표시한다. 서버 대비 아카이빙 지연(archive_lag)·임계 경고는 P2다.
async fn recoverable_item(
    profile: &Profile,
    lang: crate::i18n::Lang,
) -> crate::engine::mongo::status::CheckItem {
    use crate::engine::mongo::status::CheckItem;
    let dests = profile.effective_destinations();
    let dest = match dests.first() {
        Some(d) => *d,
        None => {
            return CheckItem::warn(
                "recoverable",
                "PITR recoverable",
                lang.sel("destination not configured", "destination 미설정"),
            )
            .with_value(lang.sel("not configured", "미설정"))
        }
    };
    let storage = match from_config(dest) {
        Ok(s) => s,
        Err(e) => {
            return CheckItem::warn(
                "recoverable",
                "PITR recoverable",
                lang.sel(
                    format!("destination access failed: {e}").as_str(),
                    format!("destination 접근 실패: {e}").as_str(),
                ),
            )
            .with_value(lang.sel("access failed", "접근 실패"))
        }
    };
    // 복구 가능 base는 실제 복원 가능한 것만 — Complete만 본다(Incomplete 최신이 복구 시점을
    // 낙관 과대보고하는 것을 막는다; 실제 restore base 선택기와 동일 기준).
    match latest_manifest_where(storage.as_ref(), |m| {
        m.status == crate::manifest::schema::BackupStatus::Complete
    })
    .await
    {
        Some(m) => match recoverable_until_rfc3339(&m) {
            Some(until) => {
                let gap = format_age_rfc3339(&until, lang);
                CheckItem::ok(
                    "recoverable",
                    "PITR recoverable",
                    lang.sel(
                        format!("recoverable until {until} (RPO gap {gap})").as_str(),
                        format!("복구가능 ~{until} (RPO 갭 {gap})").as_str(),
                    ),
                )
                .with_value(until)
            }
            // manifest는 있으나 시점 산출 불가(잘린 데이터 등).
            None => CheckItem::warn(
                "recoverable",
                "PITR recoverable",
                lang.sel(
                    "backup present but recovery point undetermined",
                    "백업은 있으나 복구 시점을 산출할 수 없습니다",
                ),
            )
            .with_value(lang.sel("unknown", "알수없음")),
        },
        None => CheckItem::warn(
            "recoverable",
            "PITR recoverable",
            lang.sel(
                "not recoverable — no backup base",
                "복구 불가 — 백업 base가 없습니다",
            ),
        )
        .with_value(lang.sel("none", "없음")),
    }
}

/// manifest에서 복구 가능 시점(RFC3339 UTC)을 산출한다.
///
/// `oplog_range`가 있으면 그 `end_ts`(마지막으로 캡처한 변경 시각, unix 초)를 정밀 시점으로
/// 쓰고, 없으면(PG/MySQL 풀 등 시간범위 미보유) `created_at`으로 근사한다. `end_ts` 변환이
/// 실패하면 `None`.
fn recoverable_until_rfc3339(m: &BackupManifest) -> Option<String> {
    if let Some(range) = &m.oplog_range {
        return chrono::DateTime::<chrono::Utc>::from_timestamp(i64::from(range.end_ts.t), 0)
            .map(|dt| dt.to_rfc3339());
    }
    Some(m.created_at.clone())
}

/// destination 쓰기 가능 여부를 작은 객체 put→delete로 점검한다(+ local 여유 공간).
///
/// 지금은 source만 점검하던 한계를 보완 — 대상이 안 닿거나 권한이 없으면 백업이 실패한다.
/// 상태는 경고(Warn)로 보고한다(env별 일시 문제로 status 전체를 exit 3으로 끊지 않도록).
async fn destination_item(
    profile: &Profile,
    lang: crate::i18n::Lang,
) -> crate::engine::mongo::status::CheckItem {
    use crate::engine::mongo::status::CheckItem;
    let dests = profile.effective_destinations();
    let dest = match dests.first() {
        Some(d) => *d,
        None => {
            return CheckItem::warn(
                "destination",
                "destination",
                lang.sel("destination not configured", "destination 미설정"),
            )
            .with_value(lang.sel("not configured", "미설정"))
        }
    };
    let storage = match from_config(dest) {
        Ok(s) => s,
        Err(e) => {
            return CheckItem::warn(
                "destination",
                "destination",
                lang.sel(
                    format!("destination creation failed: {e}").as_str(),
                    format!("destination 생성 실패: {e}").as_str(),
                ),
            )
            .with_value(lang.sel("creation failed", "생성 실패"))
        }
    };

    let kind = dest.r#type.as_deref().unwrap_or("?");
    let probe_key = ".xb-status-write-probe";
    let data: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"xb".to_vec()));
    let write_ok = storage.put_stream(probe_key, data, Some(2)).await;
    // 흔적 제거(성공·실패 무관 — best-effort).
    let _ = storage.delete(probe_key).await;

    match write_ok {
        Ok(_) => {
            let free = if kind == "local" {
                dest.path.as_deref().and_then(free_space_bytes)
            } else {
                None
            };
            let loc = dest.path.as_deref().unwrap_or(kind);
            let msg: String = match free {
                Some(b) => lang
                    .sel(
                        format!("writable ({kind}: {loc}), free {}", human_bytes(b as i64))
                            .as_str(),
                        format!("쓰기 가능({kind}: {loc}), 여유 {}", human_bytes(b as i64))
                            .as_str(),
                    )
                    .to_string(),
                None => lang
                    .sel(
                        format!("writable ({kind}: {loc})").as_str(),
                        format!("쓰기 가능({kind}: {loc})").as_str(),
                    )
                    .to_string(),
            };
            CheckItem::ok("destination", "destination", msg).with_value("OK")
        }
        Err(e) => CheckItem::warn(
            "destination",
            "destination",
            lang.sel(
                format!("write failed ({kind}): {e}").as_str(),
                format!("쓰기 실패({kind}): {e}").as_str(),
            ),
        )
        .with_value(lang.sel("write failed", "쓰기 실패")),
    }
}

/// destination의 모든 백업 중 **created_at 최신** manifest를 고른다(타입·상태 무관). 없으면 None.
async fn latest_manifest_any(storage: &dyn Storage) -> Option<BackupManifest> {
    latest_manifest_where(storage, |_| true).await
}

/// `pred`를 만족하는 백업 중 **created_at 최신** manifest를 고른다. 없으면 None.
///
/// recoverable은 실제 복원 가능한 base만 봐야 하므로 `status == Complete` 필터로 쓴다(Incomplete
/// 최신이 복구 시점을 낙관 과대보고하는 것을 막는다). last-backup 표시는 `|_| true`로 쓴다.
async fn latest_manifest_where<P>(storage: &dyn Storage, pred: P) -> Option<BackupManifest>
where
    P: Fn(&BackupManifest) -> bool,
{
    let store = ManifestStore::new(storage);
    let entries = storage.list("").await.ok()?;
    let suffix = "/manifest.json";
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_suffix(suffix)
                .filter(|id| !id.is_empty() && !id.contains('/'))
                .map(|s| s.to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();

    let mut best: Option<BackupManifest> = None;
    for id in ids {
        if let Ok(m) = store.read(&id).await {
            if !pred(&m) {
                continue;
            }
            // created_at은 RFC3339(UTC, 동일 오프셋)라 문자열 비교가 시간순과 일치한다.
            let newer = best
                .as_ref()
                .map(|b| m.created_at > b.created_at)
                .unwrap_or(true);
            if newer {
                best = Some(m);
            }
        }
    }
    best
}

/// RFC3339 시각 문자열을 현재와 비교해 사람이 읽는 경과 시간으로 만든다("2h"/"2시간" 등).
fn format_age_rfc3339(created_at: &str, lang: crate::i18n::Lang) -> String {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => {
            let secs = (chrono::Utc::now() - dt.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0) as u64;
            format_age_secs(secs, lang)
        }
        Err(_) => "?".to_string(),
    }
}

/// 경과 초를 가장 큰 단위로 근사 표기한다. 시간 단위는 짧은 표기라 언어별로 둔다(en s/m/h/d).
fn format_age_secs(s: u64, lang: crate::i18n::Lang) -> String {
    if s < 60 {
        format!("{s}{}", lang.sel("s", "초"))
    } else if s < 3600 {
        format!("{}{}", s / 60, lang.sel("m", "분"))
    } else if s < 86_400 {
        format!("{}{}", s / 3600, lang.sel("h", "시간"))
    } else {
        format!("{}{}", s / 86_400, lang.sel("d", "일"))
    }
}

/// 백업 id의 앞 8자(표시용 단축).
fn short_id(id: &str) -> &str {
    &id[..8.min(id.len())]
}

/// 경로가 속한 파일시스템의 여유 바이트(local destination 전용). 실패 시 None.
fn free_space_bytes(path: &str) -> Option<u64> {
    use std::ffi::CString;
    let c = CString::new(path).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    if rc != 0 {
        return None;
    }
    Some((st.f_bavail as u64).saturating_mul(st.f_frsize as u64))
}

/// 신호등 합산을 [`report_to_result`]에 태우기 위한 단일 항목 보고서(--all 종합용).
fn overall_placeholder(overall: CheckStatus) -> Vec<crate::engine::mongo::status::CheckItem> {
    use crate::engine::mongo::status::CheckItem;
    vec![match overall {
        CheckStatus::Ok => CheckItem::ok("all", "전체", "모든 프로파일 정상"),
        CheckStatus::Warn => CheckItem::warn("all", "전체", "경고 동반 프로파일 있음"),
        CheckStatus::Fail => CheckItem::fail("all", "전체", "실패 프로파일 있음"),
    }]
}

/// --all 표용 짧은 overall 라벨.
fn overall_short(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Ok => "OK",
        CheckStatus::Warn => "WARN",
        CheckStatus::Fail => "FAIL",
    }
}

// ───────────────────────── --all 비교(diff) 뷰 ─────────────────────────
//
// 표시 폭 정렬(display_width/pad)·색(use_color/paint/색 코드)은 [`crate::cli::table`] 공용.

/// `--all` 비교 뷰 — 왼쪽 첫 열(source 기준)과 나머지 프로파일을 차원별로 나란히 비교한다.
///
/// 각 점검 차원을 행으로, 프로파일을 열로 둔다. 셀은 비교용 짧은 값(없으면 상태 단어)이며,
/// 기준 열과 **다른 값**은 색(또는 비-TTY면 `*`)으로 강조하고 행 앞에 `Δ`를 단다. WARN/FAIL의
/// 구체 메시지는 표 아래 노트로 보존한다(요약이 detail을 가리지 않게).
fn render_comparison(reports: &[StatusReport], lang: Lang) {
    use std::collections::HashMap;

    let color = use_color();
    let baseline = &reports[0];

    // 1) 차원 키 순서 = 등장 순(첫 보고서 우선, 이후 미등장 키를 뒤에 추가).
    let mut keys: Vec<&str> = Vec::new();
    let mut labels: HashMap<&str, &str> = HashMap::new();
    for r in reports {
        for it in &r.items {
            if !keys.contains(&it.key) {
                keys.push(it.key);
            }
            labels.entry(it.key).or_insert(it.label);
        }
    }

    // 2) 셀 텍스트 조회 헬퍼: (value 우선, 없으면 상태 단어, 항목 없으면 "—").
    let cell_text = |report: &StatusReport, key: &str| -> Option<(String, CheckStatus)> {
        report.items.iter().find(|i| i.key == key).map(|it| {
            (
                it.value
                    .clone()
                    .unwrap_or_else(|| overall_short(it.status).to_string()),
                it.status,
            )
        })
    };

    // 3) 열 너비 계산(표시 폭 기준).
    let label_w = keys
        .iter()
        .map(|k| display_width(labels.get(k).copied().unwrap_or(k)))
        .chain(std::iter::once(display_width("check")))
        .max()
        .unwrap_or(8)
        .max(8);
    let col_w: Vec<usize> = reports
        .iter()
        .map(|r| {
            let header = display_width(&r.profile);
            let cells = keys
                .iter()
                .map(|k| cell_text(r, k).map(|(t, _)| display_width(&t)).unwrap_or(1));
            header.max(cells.max().unwrap_or(0)).max(6)
        })
        .collect();

    let total_w = 2 + label_w + 2 + col_w.iter().map(|w| w + 2).sum::<usize>();
    let rule = "─".repeat(total_w.min(120));

    // 4) 헤더.
    println!(
        "status --all {} {}",
        lang.sel("comparison — baseline (left):", "비교 — 기준(왼쪽):"),
        paint(&baseline.profile, &[BOLD], color)
    );
    println!("{rule}");
    print!("  {}  ", pad("check", label_w));
    for (r, w) in reports.iter().zip(&col_w) {
        print!("{}  ", pad(&r.profile, *w));
    }
    println!();
    println!("{rule}");

    // 5) 차원별 행.
    for key in &keys {
        let base = cell_text(baseline, key);
        let base_val = base.as_ref().map(|(t, _)| t.clone());
        // 행에 차이가 있는가 — 어느 비기준 열이든 값(또는 누락)이 기준과 다르면 true.
        let row_differs = reports
            .iter()
            .skip(1)
            .any(|r| cell_text(r, key).map(|(t, _)| t) != base_val);

        let gutter = if row_differs {
            paint("Δ", &[CYAN], color)
        } else {
            " ".to_string()
        };
        let label = labels.get(key).copied().unwrap_or(key);
        print!("{gutter} {}  ", pad(label, label_w));

        for (idx, (r, w)) in reports.iter().zip(&col_w).enumerate() {
            match cell_text(r, key) {
                Some((text, status)) => {
                    let differs = idx != 0 && Some(&text) != base_val.as_ref();
                    print!("{}  ", fmt_cell(&text, status, differs, *w, color));
                }
                None => print!("{}  ", pad("—", *w)),
            }
        }
        println!();
    }

    // 6) overall 행.
    println!("{rule}");
    print!("  {}  ", pad("overall", label_w));
    for (r, w) in reports.iter().zip(&col_w) {
        let txt = overall_short(r.overall);
        print!("{}  ", fmt_cell(txt, r.overall, false, *w, color));
    }
    println!();
    println!("{rule}");

    // 7) 노트 — WARN/FAIL 항목의 구체 메시지(detail 보존).
    let mut notes: Vec<String> = Vec::new();
    for r in reports {
        for it in &r.items {
            if it.status != CheckStatus::Ok {
                notes.push(format!(
                    "  {} [{}] {}: {}",
                    signal(it.status),
                    r.profile,
                    it.label,
                    it.message
                ));
            }
        }
    }
    if !notes.is_empty() {
        println!(
            "{}",
            lang.sel("notes (warning/failure detail):", "노트(경고·실패 상세):")
        );
        for n in notes {
            println!("{n}");
        }
    }
}

/// 비교 셀 렌더 — 표시 폭 패딩 후 상태색 + diff 강조(bold·underline / 비-TTY는 `*`)를 입힌다.
fn fmt_cell(text: &str, status: CheckStatus, differs: bool, width: usize, color: bool) -> String {
    // 비-TTY: 색 대신 다른 셀과 구분되도록 차이 셀에 ` *`를 덧붙인다(패딩 폭에 반영).
    if !color {
        let marked = if differs {
            format!("{text} *")
        } else {
            text.to_string()
        };
        return pad(&marked, width);
    }
    let status_code = match status {
        CheckStatus::Ok => "",
        CheckStatus::Warn => YELLOW,
        CheckStatus::Fail => RED,
    };
    let mut prefix = String::new();
    if differs {
        prefix.push_str(BOLD);
        prefix.push_str(UNDERLINE);
        // 차이가 상태색으로 안 드러나는 OK 셀은 CYAN으로 "다름"을 표시.
        prefix.push_str(if status == CheckStatus::Ok {
            CYAN
        } else {
            status_code
        });
    } else {
        prefix.push_str(status_code);
    }
    // 스타일은 **글자에만** 입히고 정렬용 패딩 공백은 RESET 뒤에 둔다 — UNDERLINE이 셀 폭
    // 전체(공백 포함)로 번져 컬럼이 통째로 밑줄처럼 보이는 가독성 저하를 막는다.
    let spaces = " ".repeat(width.saturating_sub(display_width(text)));
    if prefix.is_empty() {
        format!("{text}{spaces}")
    } else {
        format!("{prefix}{text}{RESET}{spaces}")
    }
}

/// 신호등 합산을 [`Result`]로 변환한다 — main의 exit code 매핑에 태운다.
fn report_to_result(report: &StatusReport) -> Result<()> {
    match report.overall {
        CheckStatus::Ok => Ok(()),
        CheckStatus::Warn => Err(XBackupError::Warning(format!(
            "프로파일 '{}' 점검에 경고가 있습니다(백업 가능하나 주의)",
            report.profile
        ))),
        CheckStatus::Fail => Err(XBackupError::PrecheckFailed(format!(
            "프로파일 '{}' 점검 실패 — 백업 불가 항목이 있습니다",
            report.profile
        ))),
    }
}

/// 점검 결과를 `--json`(항목 배열 + overall)으로 stdout에 출력한다.
fn render_json(report: &StatusReport) -> Result<()> {
    let json = serde_json::to_string_pretty(report)
        .map_err(|e| XBackupError::Failure(format!("status JSON 직렬화 실패: {e}")))?;
    println!("{json}");
    Ok(())
}

/// `--ns-detail`: 한 프로파일의 ns별(컬렉션/테이블) 문서 수를 모은다(엔진 자동 분기, 읽기 전용).
///
/// mongo는 [`MongoMeta::namespace_counts`](crate::engine::mongo::MongoMeta)(사용자 컬렉션),
/// PG는 [`table_counts_exact`](crate::engine::postgres::meta::table_counts_exact)(사용자 테이블)을
/// 재사용한다 — peek와 동일 기준이라 결과가 일치한다. 연결/조회 실패는 호출자가 best-effort로
/// 처리한다(한 프로파일 실패가 전체 status를 막지 않게).
async fn collect_ns_counts(config_toml: Option<&str>, profile: &str) -> Result<Vec<(String, u64)>> {
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml,
        profile_name: profile,
        overrides: &overrides,
    })?;
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri/uri_env가 없습니다",
            resolved.profile_name
        ))
    })?;
    let timeout = resolved.profile.source.connect_timeout_secs;
    match crate::engine::DbKind::from_uri(uri.expose()) {
        crate::engine::DbKind::Postgres => {
            let pg = crate::engine::postgres::meta::connect(&uri, timeout).await?;
            crate::engine::postgres::meta::table_counts_exact(pg.client()).await
        }
        crate::engine::DbKind::Mysql => {
            let mut my = crate::engine::mysql::meta::connect(&uri, timeout).await?;
            crate::engine::mysql::meta::table_counts_exact(my.conn_mut()).await
        }
        crate::engine::DbKind::Mongo => {
            let mongo = MongoMeta::connect(&uri, timeout).await?;
            mongo.namespace_counts().await
        }
    }
}

/// ns별 문서 수를 사람용 섹션으로 stdout에 출력한다(정렬 폭 맞춤, 합계 한 줄).
fn print_ns_detail_human(profile: &str, counts: &[(String, u64)], lang: Lang) {
    let color = use_color();
    println!(
        "{}",
        paint(
            &lang.sel(
                &format!("namespaces — profile: {profile} (user data)"),
                &format!("네임스페이스 — 프로파일: {profile} (사용자 데이터)")
            ),
            &[BOLD],
            color
        )
    );
    if counts.is_empty() {
        println!(
            "  {}",
            paint(
                lang.sel("(no user namespaces)", "(사용자 네임스페이스 없음)"),
                &[DIM],
                color
            )
        );
        return;
    }
    let w = counts
        .iter()
        .map(|(ns, _)| display_width(ns))
        .max()
        .unwrap_or(0);
    let mut total = 0u64;
    for (ns, c) in counts {
        total += *c;
        println!("  {}  {}", pad(ns, w), pad_left(&c.to_string(), 12));
    }
    println!(
        "  {}  {}",
        pad(lang.sel("TOTAL", "합계"), w),
        pad_left(&total.to_string(), 12)
    );
}

/// `--ns-detail` + `--json`: 보고서 JSON에 `namespaces` 배열을 끼워 한 객체로 출력한다
/// (별도 객체를 또 찍어 기계 판독을 깨지 않도록).
fn ns_counts_json(counts: &[(String, u64)]) -> serde_json::Value {
    serde_json::Value::Array(
        counts
            .iter()
            .map(|(ns, c)| serde_json::json!({ "ns": ns, "count": c }))
            .collect(),
    )
}

/// 점검 결과를 사람이 읽는 신호등 표로 stdout에 출력한다.
fn render_human(report: &StatusReport, lang: Lang) {
    let color = use_color();
    let title = format!("status check — profile: {}", report.profile);
    println!("{}", paint(&title, &[BOLD], color));

    // 라벨 칼럼은 **표시 폭**(한글 2배폭) 기준으로 패딩한다 — 문자 수(`{:<14}`)로 맞추면
    // 한글 라벨이 어긋난다(가시성 저하). [`crate::cli::table::pad`]가 표시 폭으로 정렬한다.
    let label_w = report
        .items
        .iter()
        .map(|i| display_width(i.label))
        .max()
        .unwrap_or(0);

    // 구분선 폭 = 가장 긴 라인의 표시 폭(메시지 포함). 보기 좋게 40~100으로 제한.
    // 11 = 들여쓰기(2) + 신호 "[OK  ]"(6) + 공백(1) + 라벨 뒤 공백(2).
    let line_w = report
        .items
        .iter()
        .map(|i| 11 + label_w + display_width(&i.message))
        .chain(std::iter::once(display_width(&title)))
        .max()
        .unwrap_or(60)
        .clamp(40, 100);
    let rule = "─".repeat(line_w);

    println!("{rule}");
    for item in &report.items {
        println!(
            "  {} {}  {}",
            signal_colored(item.status, color),
            pad(item.label, label_w),
            item.message,
        );
    }
    println!("{rule}");
    println!(
        "  {} {} {}",
        paint("overall:", &[BOLD], color),
        signal_colored(report.overall, color),
        overall_label(report.overall, lang),
    );
}

/// 색 입힌 상태 신호([OK]/[WARN]/[FAIL]) — OK=green, WARN=yellow, FAIL=red(굵게).
/// ANSI 코드는 표시 폭 0이라 신호 뒤 라벨 칼럼 정렬에 영향을 주지 않는다.
fn signal_colored(status: CheckStatus, color: bool) -> String {
    let (text, code) = match status {
        CheckStatus::Ok => ("[OK  ]", GREEN),
        CheckStatus::Warn => ("[WARN]", YELLOW),
        CheckStatus::Fail => ("[FAIL]", RED),
    };
    paint(text, &[BOLD, code], color)
}

/// 신호등 기호(사람용 표).
fn signal(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Ok => "[OK  ]",
        CheckStatus::Warn => "[WARN]",
        CheckStatus::Fail => "[FAIL]",
    }
}

/// 전체 신호등 라벨(종료 코드 안내 포함).
fn overall_label(status: CheckStatus, lang: Lang) -> &'static str {
    match status {
        CheckStatus::Ok => lang.sel("healthy (exit 0)", "정상 (exit 0)"),
        CheckStatus::Warn => lang.sel("with warnings (exit 4)", "경고 동반 (exit 4)"),
        CheckStatus::Fail => lang.sel(
            "failed — backup blocked (exit 3)",
            "실패 — 백업 불가 (exit 3)",
        ),
    }
}

// ───────────────────────── status --watch (라이브 모니터) ─────────────────────────

/// 한 프로파일의 라이브 스냅샷 — 틱마다 갱신되는 가벼운 메트릭(문서 수·데이터 크기).
struct LiveSnapshot {
    profile: String,
    connected: bool,
    /// 네임스페이스별 추정 문서 수(정렬).
    namespaces: Vec<(String, u64)>,
    /// 전체 문서 수(namespaces 합).
    total_docs: u64,
    /// 사용자 DB dataSize 합(바이트).
    data_size: u64,
}

impl LiveSnapshot {
    fn disconnected(profile: &str) -> Self {
        Self {
            profile: profile.to_string(),
            connected: false,
            namespaces: Vec::new(),
            total_docs: 0,
            data_size: 0,
        }
    }
}

/// 모니터가 들고 있는 라이브 연결(DB 종류별) — 끊기면 None으로 두고 다음 틱에 재연결.
enum LiveConn {
    Mongo(Option<MongoMeta>),
    Postgres(Option<crate::engine::postgres::conn::PgClient>),
    Mysql(Option<crate::engine::mysql::conn::MysqlClient>),
}

/// 프로파일별 라이브 모니터 — 드라이버 클라이언트를 재사용하고, 끊기면 다음 틱에 재연결한다.
struct Monitor {
    profile: String,
    uri: Secret,
    timeout_secs: Option<u64>,
    conn: LiveConn,
}

impl Monitor {
    /// 한 틱 폴 — 문서/행 수·데이터 크기를 가볍게(estimated) 조회한다. 조회 실패면 연결을
    /// 버리고(다음 틱 재연결) disconnected 스냅샷을 돌려준다. Mongo·PG 공통 인터페이스.
    async fn poll(&mut self) -> LiveSnapshot {
        match &mut self.conn {
            LiveConn::Mongo(meta) => {
                if meta.is_none() {
                    *meta = MongoMeta::connect(&self.uri, self.timeout_secs).await.ok();
                }
                let m = match meta {
                    Some(m) => m,
                    None => return LiveSnapshot::disconnected(&self.profile),
                };
                let namespaces = match m.namespace_counts().await {
                    Ok(n) => n,
                    Err(_) => {
                        *meta = None;
                        return LiveSnapshot::disconnected(&self.profile);
                    }
                };
                let data_size = m.data_size_bytes().await.unwrap_or(0);
                let total_docs = namespaces.iter().map(|(_, c)| c).sum();
                LiveSnapshot {
                    profile: self.profile.clone(),
                    connected: true,
                    namespaces,
                    total_docs,
                    data_size,
                }
            }
            LiveConn::Postgres(pg) => {
                use crate::engine::postgres::{conn::PgClient, meta};
                if pg.is_none() {
                    *pg = PgClient::connect(&self.uri, self.timeout_secs).await.ok();
                }
                let c = match pg {
                    Some(c) => c,
                    None => return LiveSnapshot::disconnected(&self.profile),
                };
                let namespaces = match meta::table_counts_estimated(c.client()).await {
                    Ok(n) => n,
                    Err(_) => {
                        *pg = None;
                        return LiveSnapshot::disconnected(&self.profile);
                    }
                };
                let data_size = meta::data_size_bytes(c.client()).await.unwrap_or(0);
                let total_docs = namespaces.iter().map(|(_, c)| c).sum();
                LiveSnapshot {
                    profile: self.profile.clone(),
                    connected: true,
                    namespaces,
                    total_docs,
                    data_size,
                }
            }
            LiveConn::Mysql(my) => {
                use crate::engine::mysql::{conn::MysqlClient, meta};
                if my.is_none() {
                    *my = MysqlClient::connect(&self.uri, self.timeout_secs).await.ok();
                }
                let c = match my {
                    Some(c) => c,
                    None => return LiveSnapshot::disconnected(&self.profile),
                };
                let namespaces = match meta::table_counts_estimated(c.conn_mut()).await {
                    Ok(n) => n,
                    Err(_) => {
                        *my = None;
                        return LiveSnapshot::disconnected(&self.profile);
                    }
                };
                let data_size = meta::data_size_bytes(c.conn_mut()).await.unwrap_or(0);
                let total_docs = namespaces.iter().map(|(_, c)| c).sum();
                LiveSnapshot {
                    profile: self.profile.clone(),
                    connected: true,
                    namespaces,
                    total_docs,
                    data_size,
                }
            }
        }
    }
}

/// 프로파일 목록의 모니터를 만든다(URI 해석 — config 오류는 루프 진입 전에 실패시킨다).
fn build_monitors(config_toml: Option<&str>, profiles: &[String]) -> Result<Vec<Monitor>> {
    let overrides = collect_overrides_from_process();
    let mut monitors = Vec::with_capacity(profiles.len());
    for p in profiles {
        let resolved = ResolvedConfig::build(MergeInput {
            config_toml,
            profile_name: p,
            overrides: &overrides,
        })?;
        let uri = resolved.resolved_uri.clone().ok_or_else(|| {
            XBackupError::Config(format!(
                "프로파일 '{}'에 source.uri/uri_env가 없습니다",
                resolved.profile_name
            ))
        })?;
        let conn = match crate::engine::DbKind::from_uri(uri.expose()) {
            crate::engine::DbKind::Postgres => LiveConn::Postgres(None),
            crate::engine::DbKind::Mongo => LiveConn::Mongo(None),
            crate::engine::DbKind::Mysql => LiveConn::Mysql(None),
        };
        monitors.push(Monitor {
            profile: resolved.profile_name.clone(),
            uri,
            timeout_secs: resolved.profile.source.connect_timeout_secs,
            conn,
        });
    }
    Ok(monitors)
}

/// 라이브 모드 진입 — 주기 갱신하며 변경량(Δ)을 추적한다. Ctrl-C 또는 `--count` 도달 시 종료(exit 0).
async fn handle_watch(config_toml: Option<&str>, args: &StatusArgs, lang: Lang) -> Result<()> {
    if args.json {
        return Err(XBackupError::Usage(
            "--watch는 --json과 함께 쓸 수 없습니다(라이브 표시 전용)".into(),
        ));
    }
    // 대상 프로파일 — --all이면 config의 모든 프로파일, 아니면 단일.
    let profiles: Vec<String> = if args.all {
        let raw = config_toml
            .ok_or_else(|| XBackupError::Usage("--all에는 config 파일이 필요합니다".into()))?;
        let config = crate::config::file::Config::from_toml_str(raw)?;
        let mut names: Vec<String> = config.profiles.keys().cloned().collect();
        names.sort();
        if names.is_empty() {
            return Err(XBackupError::Usage(
                "config에 프로파일이 없습니다([profiles.<name>])".into(),
            ));
        }
        names
    } else {
        let p = args
            .profile
            .clone()
            .ok_or_else(|| XBackupError::Usage("--profile 또는 --all이 필요합니다".into()))?;
        vec![p]
    };

    let interval = Duration::from_secs_f64(args.interval.max(0.2));
    let color = use_color();
    let tty = std::io::stdout().is_terminal();
    let mut monitors = build_monitors(config_toml, &profiles)?;

    if tty {
        print!("\x1b[?25l"); // 커서 숨김.
        let _ = std::io::stdout().flush();
    }

    let mut prev: std::collections::HashMap<String, LiveSnapshot> =
        std::collections::HashMap::new();
    let mut tick: u64 = 0;
    loop {
        tick += 1;
        let mut snaps = Vec::with_capacity(monitors.len());
        for m in &mut monitors {
            snaps.push(m.poll().await);
        }

        let frame = render_watch_frame(&snaps, &prev, args.all, tick, args.interval, color, lang);
        if tty {
            // 화면 지우고 홈으로 — watch처럼 제자리 갱신.
            print!("\x1b[2J\x1b[H{frame}");
        } else {
            println!("{frame}");
        }
        let _ = std::io::stdout().flush();

        prev = snaps.into_iter().map(|s| (s.profile.clone(), s)).collect();

        if args.count != 0 && tick >= args.count {
            break;
        }
        tokio::select! {
            _ = tokio::time::sleep(interval) => {}
            _ = tokio::signal::ctrl_c() => { break; }
        }
    }

    if tty {
        println!("\x1b[?25h"); // 커서 복원 + 줄바꿈.
        let _ = std::io::stdout().flush();
    }
    Ok(())
}

/// 문서 수 변화량 셀 텍스트와 색 — 첫 틱(기준 없음)은 `—`(흐림).
fn delta_docs(cur: u64, prev: Option<u64>) -> (String, &'static str) {
    match prev {
        None => ("—".to_string(), DIM),
        Some(p) => {
            let d = cur as i64 - p as i64;
            if d > 0 {
                (format!("+{d}"), GREEN)
            } else if d < 0 {
                (d.to_string(), RED)
            } else {
                ("0".to_string(), DIM)
            }
        }
    }
}

/// 바이트 변화량 셀 텍스트와 색 — 첫 틱은 `—`(흐림).
fn delta_bytes(cur: u64, prev: Option<u64>) -> (String, &'static str) {
    match prev {
        None => ("—".to_string(), DIM),
        Some(p) => {
            let d = cur as i64 - p as i64;
            if d > 0 {
                (format!("+{}", human_bytes(d)), GREEN)
            } else if d < 0 {
                (format!("-{}", human_bytes(-d)), RED)
            } else {
                ("0".to_string(), DIM)
            }
        }
    }
}

/// 폭 정렬 + 색을 입힌 셀(빈 코드면 무채색).
fn wcell(text: &str, width: usize, align: Align, code: &str, color: bool) -> String {
    let padded = match align {
        Align::Left => pad(text, width),
        Align::Right => pad_left(text, width),
    };
    if code.is_empty() {
        padded
    } else {
        paint(&padded, &[code], color)
    }
}

/// 한 틱의 화면 프레임을 만든다(단일/--all 분기).
fn render_watch_frame(
    snaps: &[LiveSnapshot],
    prev: &std::collections::HashMap<String, LiveSnapshot>,
    all: bool,
    tick: u64,
    interval: f64,
    color: bool,
    lang: Lang,
) -> String {
    let now = chrono::Local::now().format("%H:%M:%S");
    let scope = if all {
        "--all".to_string()
    } else {
        snaps.first().map(|s| s.profile.clone()).unwrap_or_default()
    };
    let header = format!(
        "status --watch {scope} · {} · {now} · #{tick}    {}",
        lang.sel(&format!("every {interval}s"), &format!("매 {interval}s")),
        lang.sel("(Ctrl-C to quit)", "(Ctrl-C 종료)")
    );
    if all {
        render_watch_all(snaps, prev, &header, color, lang)
    } else if let Some(s) = snaps.first() {
        render_watch_single(s, prev.get(&s.profile), &header, color, lang)
    } else {
        header
    }
}

/// 단일 프로파일 라이브 — 네임스페이스별 문서 수 + Δ, 하단에 합계·데이터·연결.
fn render_watch_single(
    s: &LiveSnapshot,
    prev: Option<&LiveSnapshot>,
    header: &str,
    color: bool,
    lang: Lang,
) -> String {
    use std::collections::HashMap;
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');

    if !s.connected {
        out.push_str(&paint(
            lang.sel(
                "  ● disconnected — reconnecting",
                "  ● 연결 끊김 — 재연결 시도 중",
            ),
            &[RED],
            color,
        ));
        return out;
    }

    let prev_ns: HashMap<&str, u64> = prev
        .map(|p| p.namespaces.iter().map(|(n, c)| (n.as_str(), *c)).collect())
        .unwrap_or_default();

    // 행: (ns, 문서 수 문자열, Δ 문자열, Δ 색).
    let mut rows: Vec<(String, String, String, &'static str)> = Vec::new();
    for (ns, c) in &s.namespaces {
        let prevc = prev.map(|_| prev_ns.get(ns.as_str()).copied().unwrap_or(0));
        let (d, code) = delta_docs(*c, prevc);
        rows.push((ns.clone(), c.to_string(), d, code));
    }

    let w_ns = rows
        .iter()
        .map(|r| display_width(&r.0))
        .chain(std::iter::once(display_width("namespace")))
        .max()
        .unwrap_or(12);
    let w_cnt = rows
        .iter()
        .map(|r| display_width(&r.1))
        .chain(std::iter::once(display_width("documents")))
        .max()
        .unwrap_or(4);
    let w_dlt = rows
        .iter()
        .map(|r| display_width(&r.2))
        .chain(std::iter::once(1))
        .max()
        .unwrap_or(4)
        .max(4);

    let rule = "─".repeat((2 + w_ns + 2 + w_cnt + 2 + w_dlt).min(100));
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&format!(
        "  {}  {}  {}\n",
        pad("namespace", w_ns),
        pad_left("documents", w_cnt),
        pad_left("Δ", w_dlt)
    ));
    if rows.is_empty() {
        out.push_str(lang.sel("  (no user data)\n", "  (사용자 데이터 없음)\n"));
    }
    for (ns, cnt, dlt, code) in &rows {
        out.push_str(&format!(
            "  {}  {}  {}\n",
            pad(ns, w_ns),
            pad_left(cnt, w_cnt),
            wcell(dlt, w_dlt, Align::Right, code, color),
        ));
    }
    out.push_str(&rule);
    out.push('\n');

    // 하단 요약 — 합계 문서/데이터/연결, 각각 Δ.
    let (td, tdc) = delta_docs(s.total_docs, prev.map(|p| p.total_docs));
    let (sz, szc) = delta_bytes(s.data_size, prev.map(|p| p.data_size));
    out.push_str(&format!(
        "  total documents: {} ({})    data: {} ({})    connection: {}",
        s.total_docs,
        paint(&td, &[tdc], color),
        human_bytes(s.data_size as i64),
        paint(&sz, &[szc], color),
        paint("OK", &[GREEN], color),
    ));
    out
}

/// --all 라이브 — 프로파일별 한 행(연결·문서·Δ·데이터·Δ)으로 모든 자원을 동시에 추적.
fn render_watch_all(
    snaps: &[LiveSnapshot],
    prev: &std::collections::HashMap<String, LiveSnapshot>,
    header: &str,
    color: bool,
    _lang: Lang,
) -> String {
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');

    // 행: (profile, 연결, 연결색, 문서, Δ문서, Δ색, 데이터, Δ데이터, Δ색).
    struct Row {
        profile: String,
        conn: &'static str,
        conn_code: &'static str,
        docs: String,
        ddocs: String,
        ddocs_code: &'static str,
        size: String,
        dsize: String,
        dsize_code: &'static str,
    }
    let mut rows: Vec<Row> = Vec::with_capacity(snaps.len());
    for s in snaps {
        let p = prev.get(&s.profile);
        if !s.connected {
            rows.push(Row {
                profile: s.profile.clone(),
                conn: "down",
                conn_code: RED,
                docs: "—".into(),
                ddocs: "—".into(),
                ddocs_code: DIM,
                size: "—".into(),
                dsize: "—".into(),
                dsize_code: DIM,
            });
            continue;
        }
        let (dd, ddc) = delta_docs(s.total_docs, p.map(|x| x.total_docs));
        let (ds, dsc) = delta_bytes(s.data_size, p.map(|x| x.data_size));
        rows.push(Row {
            profile: s.profile.clone(),
            conn: "OK",
            conn_code: GREEN,
            docs: s.total_docs.to_string(),
            ddocs: dd,
            ddocs_code: ddc,
            size: human_bytes(s.data_size as i64),
            dsize: ds,
            dsize_code: dsc,
        });
    }

    let w = |sel: &dyn Fn(&Row) -> &str, head: &str| -> usize {
        rows.iter()
            .map(|r| display_width(sel(r)))
            .chain(std::iter::once(display_width(head)))
            .max()
            .unwrap_or(display_width(head))
    };
    let w_p = w(&|r| &r.profile, "profile").max(6);
    let w_c = w(&|r| r.conn, "connection").max(4);
    let w_d = w(&|r| &r.docs, "documents").max(4);
    let w_dd = w(&|r| &r.ddocs, "Δ").max(4);
    let w_s = w(&|r| &r.size, "data").max(6);
    let w_ds = w(&|r| &r.dsize, "Δ").max(6);

    let rule = "─".repeat((2 + w_p + 2 + w_c + 2 + w_d + 2 + w_dd + 2 + w_s + 2 + w_ds).min(110));
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}  {}\n",
        pad("profile", w_p),
        pad("connection", w_c),
        pad_left("documents", w_d),
        pad_left("Δ", w_dd),
        pad_left("data", w_s),
        pad_left("Δ", w_ds),
    ));
    for r in &rows {
        out.push_str(&format!(
            "  {}  {}  {}  {}  {}  {}\n",
            pad(&r.profile, w_p),
            wcell(r.conn, w_c, Align::Left, r.conn_code, color),
            pad_left(&r.docs, w_d),
            wcell(&r.ddocs, w_dd, Align::Right, r.ddocs_code, color),
            pad_left(&r.size, w_s),
            wcell(&r.dsize, w_ds, Align::Right, r.dsize_code, color),
        ));
    }
    out.push_str(&rule);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mongo::status::CheckItem;

    /// 최소 필드 manifest를 serde로 구성한다(recoverable 시점 산출 테스트용).
    fn manifest_with_oplog_end(oplog_end: Option<u32>) -> BackupManifest {
        let mut v = serde_json::json!({
            "format_version": 1,
            "id": "0190-test",
            "created_at": "2026-06-12T13:00:00+00:00",
            "backup_type": "full",
            "topology": "replica_set",
            "server_version": "7.0.0",
            "selective": false,
            "original_size_bytes": 0,
            "stored_size_bytes": 0,
            "checksum_sha256": "abc",
            "status": "complete"
        });
        if let Some(t) = oplog_end {
            v["oplog_range"] = serde_json::json!({
                "start_ts": { "t": t, "i": 1 },
                "end_ts": { "t": t, "i": 5 }
            });
        }
        serde_json::from_value(v).expect("manifest 역직렬화")
    }

    /// oplog_range가 있으면 end_ts(정밀 시점)를 복구 시점으로 쓴다.
    #[test]
    fn recoverable_uses_oplog_end_when_present() {
        let m = manifest_with_oplog_end(Some(1_781_272_133));
        let got = recoverable_until_rfc3339(&m).unwrap();
        let expected = chrono::DateTime::<chrono::Utc>::from_timestamp(1_781_272_133, 0)
            .unwrap()
            .to_rfc3339();
        assert_eq!(got, expected);
    }

    /// oplog_range가 없으면(PG/MySQL 풀 등) created_at으로 근사한다.
    #[test]
    fn recoverable_falls_back_to_created_at() {
        let m = manifest_with_oplog_end(None);
        assert_eq!(
            recoverable_until_rfc3339(&m).as_deref(),
            Some("2026-06-12T13:00:00+00:00")
        );
    }

    /// F2 회귀: recoverable은 Complete만 골라야 한다 — 더 최신인 Incomplete가 있어도
    /// 복구 시점으로 오보하지 않는다(latest_manifest_where(Complete)).
    #[tokio::test]
    async fn recoverable_where_skips_newer_incomplete() {
        fn manifest(id: &str, created: &str, status: &str) -> BackupManifest {
            let v = serde_json::json!({
                "format_version": 1, "id": id, "created_at": created,
                "backup_type": "full", "topology": "replica_set", "server_version": "7.0.0",
                "selective": false, "original_size_bytes": 0, "stored_size_bytes": 0,
                "checksum_sha256": "abc", "status": status
            });
            serde_json::from_value(v).unwrap()
        }
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::storage::LocalFs::new(dir.path()).unwrap();
        let store = crate::manifest::store::ManifestStore::new(&fs);
        // Complete는 과거(10:00), Incomplete는 더 최신(12:00).
        store
            .write(&manifest("c1", "2026-06-12T10:00:00+00:00", "complete"))
            .await
            .unwrap();
        store
            .write(&manifest("i1", "2026-06-12T12:00:00+00:00", "incomplete"))
            .await
            .unwrap();

        // Complete 필터: 더 최신 Incomplete를 건너뛰고 Complete(c1)를 고른다.
        let complete = latest_manifest_where(&fs, |m| {
            m.status == crate::manifest::schema::BackupStatus::Complete
        })
        .await
        .unwrap();
        assert_eq!(complete.id, "c1");
        // 무필터(last-backup 표시용)는 최신 Incomplete(i1)를 고른다 — 기존 동작 유지.
        assert_eq!(latest_manifest_any(&fs).await.unwrap().id, "i1");
    }

    #[test]
    fn ok_report_maps_to_ok_result() {
        let report = StatusReport::new("p", vec![CheckItem::ok("a", "A", "")]);
        assert!(report_to_result(&report).is_ok());
    }

    #[test]
    fn warn_report_maps_to_exit_4() {
        let report = StatusReport::new("p", vec![CheckItem::warn("a", "A", "")]);
        let err = report_to_result(&report).unwrap_err();
        assert_eq!(err.exit_code(), 4);
    }

    #[test]
    fn fail_report_maps_to_exit_3() {
        let report = StatusReport::new(
            "p",
            vec![CheckItem::fail("topology", "토폴로지", "샤딩 감지")],
        );
        let err = report_to_result(&report).unwrap_err();
        assert_eq!(err.exit_code(), 3);
    }

    #[test]
    fn json_renders_without_panic() {
        let report = StatusReport::new(
            "prod",
            vec![
                CheckItem::ok("connection", "연결·인증", "연결 성공"),
                CheckItem::fail("privileges", "권한", "find 누락"),
            ],
        );
        // 직렬화 성공만 확인(출력 자체는 stdout).
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"overall\":\"fail\""));
        assert!(json.contains("\"connection\""));
    }

    #[test]
    fn json_includes_value_when_present() {
        let report = StatusReport::new(
            "prod",
            vec![CheckItem::ok("version", "버전 정합", "서버=7.0.35").with_value("7.0.35")],
        );
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"value\":\"7.0.35\""));
    }

    // display_width/pad 자체 검증은 cli::table 모듈 테스트에 있다.

    #[test]
    fn fmt_cell_color_marks_diff_with_ansi() {
        let s = fmt_cell("7.0.35", CheckStatus::Ok, true, 8, true);
        assert!(s.contains(BOLD) && s.contains(UNDERLINE) && s.contains(RESET));
        // 정렬 패딩 공백은 RESET 뒤에 와야 한다 — UNDERLINE이 공백까지 번지면 컬럼이 통째로
        // 밑줄처럼 보여 가독성이 떨어진다(회귀 방지).
        let (styled, tail) = s.split_once(RESET).expect("RESET 존재");
        assert!(
            !tail.is_empty() && tail.bytes().all(|b| b == b' '),
            "패딩은 RESET 뒤 공백만"
        );
        assert!(!styled.contains(' '), "스타일 구간엔 공백이 없어야 한다");
    }

    #[test]
    fn fmt_cell_nocolor_marks_diff_with_star() {
        let s = fmt_cell("7.0.35", CheckStatus::Ok, true, 12, false);
        assert!(s.contains('*'));
        assert!(!s.contains('\x1b'), "비-TTY는 ANSI를 쓰지 않는다");
    }

    #[test]
    fn fmt_cell_nocolor_same_has_no_marker() {
        let s = fmt_cell("wiredTiger", CheckStatus::Ok, false, 12, false);
        assert!(!s.contains('*'));
        assert!(!s.contains('\x1b'));
    }

    #[test]
    fn delta_docs_signs_and_baseline() {
        // 첫 틱(기준 없음)은 — (흐림).
        assert_eq!(delta_docs(10, None), ("—".to_string(), DIM));
        // 증가/감소/동일.
        assert_eq!(delta_docs(15, Some(10)), ("+5".to_string(), GREEN));
        assert_eq!(delta_docs(7, Some(10)), ("-3".to_string(), RED));
        assert_eq!(delta_docs(10, Some(10)), ("0".to_string(), DIM));
    }

    #[test]
    fn delta_bytes_signs_and_baseline() {
        assert_eq!(delta_bytes(2048, None), ("—".to_string(), DIM));
        let (txt, code) = delta_bytes(2048, Some(1024));
        assert!(txt.starts_with('+') && code == GREEN);
        let (txt, code) = delta_bytes(1024, Some(2048));
        assert!(txt.starts_with('-') && code == RED);
        assert_eq!(delta_bytes(1024, Some(1024)), ("0".to_string(), DIM));
    }

    #[test]
    fn age_scales_units() {
        use crate::i18n::Lang;
        // en은 짧은 단위(s/m/h/d), ko는 한글 단위.
        assert_eq!(format_age_secs(30, Lang::En), "30s");
        assert_eq!(format_age_secs(150, Lang::En), "2m");
        assert_eq!(format_age_secs(7200, Lang::En), "2h");
        assert_eq!(format_age_secs(172800, Lang::En), "2d");
        assert_eq!(format_age_secs(30, Lang::Ko), "30초");
        assert_eq!(format_age_secs(7200, Lang::Ko), "2시간");
    }

    #[test]
    fn short_id_truncates_to_eight() {
        assert_eq!(short_id("019ec4a4-1234-7000-abcd"), "019ec4a4");
        assert_eq!(short_id("abc"), "abc");
    }
}
