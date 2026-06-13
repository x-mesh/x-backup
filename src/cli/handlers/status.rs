//! `status` 서브커맨드 핸들러 — 대상 서버 상태를 읽기 전용으로 점검(FR-8, R14).
//!
//! config 로드 → URI 해석 → [`StatusChecker`] 전체 점검 → 신호등 요약 출력(사람/`--json`).
//! 종료 코드는 신호등 합산으로 결정한다(PRD §9): fail=3(사전 점검 실패), warn=4(경고
//! 동반 성공), 모두 ok면 0.
//!
//! 무부작용·읽기 전용이다 — 어떤 쓰기/변경도 하지 않는다(PRD §FR-8 동작 요건).

use std::path::PathBuf;

use crate::cli::args::StatusArgs;
use crate::cli::table::{
    display_width, pad, paint, use_color, BOLD, CYAN, RED, RESET, UNDERLINE, YELLOW,
};
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::engine::mongo::status::{CheckStatus, StatusChecker, StatusReport};
use crate::error::{Result, XBackupError};

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

    let report =
        match StatusChecker::connect(&uri, resolved.profile.source.connect_timeout_secs).await {
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
        };
    Ok(report)
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
}
