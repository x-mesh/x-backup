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
use crate::manifest::schema::{BackupManifest, BackupType};
use crate::manifest::ManifestStore;
use crate::storage::{from_config, BoxAsyncRead, Storage};

/// backup 자동 사전 점검에서 쓰는 mongodump 실행파일 이름(backup 핸들러와 동일 기본값).
pub const DEFAULT_MONGODUMP: &str = "mongodump";

/// `status` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: StatusArgs) -> Result<()> {
    // config 파일은 한 번만 읽는다(--all은 여러 프로파일에 재사용).
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };

    // 라이브 모드 — 주기 갱신하며 변경량(Δ)을 추적한다(단일/--all 모두 지원).
    if args.watch {
        return handle_watch(config_toml.as_deref(), &args).await;
    }

    if args.all {
        return handle_all(config_toml.as_deref(), args.json).await;
    }

    // 단일 프로파일(--all 아니면 --profile 필수 — clap이 강제).
    let profile = args
        .profile
        .as_deref()
        .ok_or_else(|| XBackupError::Usage("--profile 또는 --all이 필요합니다".into()))?;
    let report = build_report(config_toml.as_deref(), profile).await?;

    // 출력 — --json 구조화 또는 사람용 표.
    if args.json {
        render_json(&report)?;
    } else {
        render_human(&report);
    }

    // 신호등 → 종료 코드. fail이면 PrecheckFailed(exit 3), warn이면 Warning(exit 4), ok면 0.
    report_to_result(&report)
}

/// `--all` — config의 모든 프로파일을 점검한다. 사람용 출력은 **프로파일별 전체 상세**
/// (단일 `status`와 동일한 신호등 표)를 차례로 찍은 뒤, 마지막에 한 줄씩 종합 표를 붙인다.
/// 종료 코드는 최악 신호등을 따른다. `-v`는 로그 레벨일 뿐 이 보고서 상세도와 무관하다.
async fn handle_all(config_toml: Option<&str>, json: bool) -> Result<()> {
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

    let mut reports = Vec::with_capacity(names.len());
    for name in &names {
        reports.push(build_report(config_toml, name).await?);
    }

    // 출력.
    if json {
        let items: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "profile": r.profile,
                    "overall": format!("{:?}", r.overall).to_lowercase(),
                    "items": r.items.iter().map(|i| serde_json::json!({
                        "key": i.key,
                        "label": i.label,
                        "status": format!("{:?}", i.status).to_lowercase(),
                        "value": i.value,
                        "message": i.message,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "profiles": items }));
    } else if reports.len() == 1 {
        // 프로파일이 하나면 비교 의미가 없다 — 단일 status 상세 표 그대로.
        render_human(&reports[0]);
    } else {
        // 둘 이상이면 source(왼쪽=첫 열) 기준 비교 표.
        render_comparison(&reports);
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
async fn build_report(config_toml: Option<&str>, profile: &str) -> Result<StatusReport> {
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

    // DB 종류 분기 — postgres URI면 PG status, 그 외는 Mongo status.
    let report = if crate::engine::DbKind::from_uri(uri.expose()) == crate::engine::DbKind::Postgres
    {
        crate::engine::postgres::status::full_report(&resolved.profile_name, &uri, timeout).await
    } else {
        match StatusChecker::connect(&uri, timeout).await {
            Ok(checker) => {
                checker
                    .full_report(
                        &resolved.profile_name,
                        DEFAULT_MONGODUMP,
                        &interval,
                        prefer_secondary,
                    )
                    .await
            }
            Err(e) => StatusReport::new(
                &resolved.profile_name,
                vec![crate::engine::mongo::status::CheckItem::fail(
                    "connection",
                    "연결·인증",
                    format!("연결 준비 실패: {e}"),
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
    items.push(last_backup_item(&resolved.profile).await);
    items.push(destination_item(&resolved.profile).await);
    Ok(StatusReport::new(name, items))
}

/// destination의 최신 manifest를 읽어 "마지막 백업" 항목을 만든다(나이·타입·크기). 무변경.
async fn last_backup_item(profile: &Profile) -> crate::engine::mongo::status::CheckItem {
    use crate::engine::mongo::status::CheckItem;
    let dests = profile.effective_destinations();
    let dest = match dests.first() {
        Some(d) => *d,
        None => {
            return CheckItem::warn(
                "last_backup",
                "마지막 백업",
                "destination 미설정".to_string(),
            )
            .with_value("미설정")
        }
    };
    let storage = match from_config(dest) {
        Ok(s) => s,
        Err(e) => {
            return CheckItem::warn(
                "last_backup",
                "마지막 백업",
                format!("destination 접근 실패: {e}"),
            )
            .with_value("접근 실패")
        }
    };
    match latest_manifest_any(storage.as_ref()).await {
        Some(m) => {
            let age = format_age_rfc3339(&m.created_at);
            let typ = match m.backup_type {
                BackupType::Full => "full",
                BackupType::Incremental => "incr",
            };
            CheckItem::ok(
                "last_backup",
                "마지막 백업",
                format!(
                    "{typ}, {}, {age} 전 (id {})",
                    human_bytes(m.stored_size_bytes as i64),
                    short_id(&m.id)
                ),
            )
            .with_value(format!("{age} 전"))
        }
        None => CheckItem::warn(
            "last_backup",
            "마지막 백업",
            "백업 이력이 없습니다".to_string(),
        )
        .with_value("없음"),
    }
}

/// destination 쓰기 가능 여부를 작은 객체 put→delete로 점검한다(+ local 여유 공간).
///
/// 지금은 source만 점검하던 한계를 보완 — 대상이 안 닿거나 권한이 없으면 백업이 실패한다.
/// 상태는 경고(Warn)로 보고한다(env별 일시 문제로 status 전체를 exit 3으로 끊지 않도록).
async fn destination_item(profile: &Profile) -> crate::engine::mongo::status::CheckItem {
    use crate::engine::mongo::status::CheckItem;
    let dests = profile.effective_destinations();
    let dest = match dests.first() {
        Some(d) => *d,
        None => {
            return CheckItem::warn(
                "destination",
                "destination",
                "destination 미설정".to_string(),
            )
            .with_value("미설정")
        }
    };
    let storage = match from_config(dest) {
        Ok(s) => s,
        Err(e) => {
            return CheckItem::warn(
                "destination",
                "destination",
                format!("destination 생성 실패: {e}"),
            )
            .with_value("생성 실패")
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
            let extra = if kind == "local" {
                dest.path
                    .as_deref()
                    .and_then(free_space_bytes)
                    .map(|b| format!(", 여유 {}", human_bytes(b as i64)))
                    .unwrap_or_default()
            } else {
                String::new()
            };
            let loc = dest.path.as_deref().unwrap_or(kind);
            CheckItem::ok(
                "destination",
                "destination",
                format!("쓰기 가능({kind}: {loc}){extra}"),
            )
            .with_value("OK")
        }
        Err(e) => CheckItem::warn(
            "destination",
            "destination",
            format!("쓰기 실패({kind}): {e}"),
        )
        .with_value("쓰기 실패"),
    }
}

/// destination의 모든 백업 중 **created_at 최신** manifest를 고른다(타입 무관). 없으면 None.
async fn latest_manifest_any(storage: &dyn Storage) -> Option<BackupManifest> {
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

/// RFC3339 시각 문자열을 현재와 비교해 사람이 읽는 경과 시간으로 만든다("2시간" 등).
fn format_age_rfc3339(created_at: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(created_at) {
        Ok(dt) => {
            let secs = (chrono::Utc::now() - dt.with_timezone(&chrono::Utc))
                .num_seconds()
                .max(0) as u64;
            format_age_secs(secs)
        }
        Err(_) => "?".to_string(),
    }
}

/// 경과 초를 가장 큰 단위로 근사 표기한다.
fn format_age_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}초")
    } else if s < 3600 {
        format!("{}분", s / 60)
    } else if s < 86_400 {
        format!("{}시간", s / 3600)
    } else {
        format!("{}일", s / 86_400)
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
fn render_comparison(reports: &[StatusReport]) {
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
        .chain(std::iter::once(display_width("점검")))
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
        "status --all 비교 — 기준(왼쪽): {}",
        paint(&baseline.profile, &[BOLD], color)
    );
    println!("{rule}");
    print!("  {}  ", pad("점검", label_w));
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
        println!("노트(경고·실패 상세):");
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
    let padded = pad(text, width);
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
    if prefix.is_empty() {
        padded
    } else {
        format!("{prefix}{padded}{RESET}")
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

/// 점검 결과를 사람이 읽는 신호등 표로 stdout에 출력한다.
fn render_human(report: &StatusReport) {
    println!("status 점검 — 프로파일: {}", report.profile);
    println!("{:-<60}", "");
    for item in &report.items {
        println!(
            "  {} {:<14} {}",
            signal(item.status),
            item.label,
            item.message
        );
    }
    println!("{:-<60}", "");
    println!(
        "  전체: {} {}",
        signal(report.overall),
        overall_label(report.overall)
    );
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
fn overall_label(status: CheckStatus) -> &'static str {
    match status {
        CheckStatus::Ok => "정상 (exit 0)",
        CheckStatus::Warn => "경고 동반 (exit 4)",
        CheckStatus::Fail => "실패 — 백업 불가 (exit 3)",
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

/// 프로파일별 라이브 모니터 — 드라이버 클라이언트를 재사용하고, 끊기면 다음 틱에 재연결한다.
struct Monitor {
    profile: String,
    uri: Secret,
    timeout_secs: Option<u64>,
    meta: Option<MongoMeta>,
}

impl Monitor {
    /// 한 틱 폴 — 문서 수·데이터 크기를 가볍게(estimatedDocumentCount·dbStats) 조회한다.
    /// 조회 실패면 연결을 버리고(다음 틱 재연결) disconnected 스냅샷을 돌려준다.
    async fn poll(&mut self) -> LiveSnapshot {
        if self.meta.is_none() {
            self.meta = MongoMeta::connect(&self.uri, self.timeout_secs).await.ok();
        }
        let meta = match &self.meta {
            Some(m) => m,
            None => return LiveSnapshot::disconnected(&self.profile),
        };
        let namespaces = match meta.namespace_counts().await {
            Ok(n) => n,
            Err(_) => {
                self.meta = None;
                return LiveSnapshot::disconnected(&self.profile);
            }
        };
        let data_size = meta.data_size_bytes().await.unwrap_or(0);
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
        monitors.push(Monitor {
            profile: resolved.profile_name.clone(),
            uri,
            timeout_secs: resolved.profile.source.connect_timeout_secs,
            meta: None,
        });
    }
    Ok(monitors)
}

/// 라이브 모드 진입 — 주기 갱신하며 변경량(Δ)을 추적한다. Ctrl-C 또는 `--count` 도달 시 종료(exit 0).
async fn handle_watch(config_toml: Option<&str>, args: &StatusArgs) -> Result<()> {
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

        let frame = render_watch_frame(&snaps, &prev, args.all, tick, args.interval, color);
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
) -> String {
    let now = chrono::Local::now().format("%H:%M:%S");
    let scope = if all {
        "--all".to_string()
    } else {
        snaps.first().map(|s| s.profile.clone()).unwrap_or_default()
    };
    let header =
        format!("status --watch {scope} · 매 {interval}s · {now} · #{tick}    (Ctrl-C 종료)");
    if all {
        render_watch_all(snaps, prev, &header, color)
    } else if let Some(s) = snaps.first() {
        render_watch_single(s, prev.get(&s.profile), &header, color)
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
) -> String {
    use std::collections::HashMap;
    let mut out = String::new();
    out.push_str(header);
    out.push('\n');

    if !s.connected {
        out.push_str(&paint("  ● 연결 끊김 — 재연결 시도 중", &[RED], color));
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
        .chain(std::iter::once(display_width("네임스페이스")))
        .max()
        .unwrap_or(12);
    let w_cnt = rows
        .iter()
        .map(|r| display_width(&r.1))
        .chain(std::iter::once(display_width("문서")))
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
        pad("네임스페이스", w_ns),
        pad_left("문서", w_cnt),
        pad_left("Δ", w_dlt)
    ));
    if rows.is_empty() {
        out.push_str("  (사용자 데이터 없음)\n");
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
        "  합계 문서: {} ({})    데이터: {} ({})    연결: {}",
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
                conn: "끊김",
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
    let w_c = w(&|r| r.conn, "연결").max(4);
    let w_d = w(&|r| &r.docs, "문서").max(4);
    let w_dd = w(&|r| &r.ddocs, "Δ").max(4);
    let w_s = w(&|r| &r.size, "데이터").max(6);
    let w_ds = w(&|r| &r.dsize, "Δ").max(6);

    let rule = "─".repeat((2 + w_p + 2 + w_c + 2 + w_d + 2 + w_dd + 2 + w_s + 2 + w_ds).min(110));
    out.push_str(&rule);
    out.push('\n');
    out.push_str(&format!(
        "  {}  {}  {}  {}  {}  {}\n",
        pad("profile", w_p),
        pad("연결", w_c),
        pad_left("문서", w_d),
        pad_left("Δ", w_dd),
        pad_left("데이터", w_s),
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
        assert_eq!(format_age_secs(30), "30초");
        assert_eq!(format_age_secs(150), "2분");
        assert_eq!(format_age_secs(7200), "2시간");
        assert_eq!(format_age_secs(172800), "2일");
    }

    #[test]
    fn short_id_truncates_to_eight() {
        assert_eq!(short_id("019ec4a4-1234-7000-abcd"), "019ec4a4");
        assert_eq!(short_id("abc"), "abc");
    }
}
