//! `status` 서브커맨드 핸들러 — 대상 서버 상태를 읽기 전용으로 점검(FR-8, R14).
//!
//! config 로드 → URI 해석 → [`StatusChecker`] 전체 점검 → 신호등 요약 출력(사람/`--json`).
//! 종료 코드는 신호등 합산으로 결정한다(PRD §9): fail=3(사전 점검 실패), warn=4(경고
//! 동반 성공), 모두 ok면 0.
//!
//! 무부작용·읽기 전용이다 — 어떤 쓰기/변경도 하지 않는다(PRD §FR-8 동작 요건).

use std::path::PathBuf;

use crate::cli::args::StatusArgs;
use crate::config::env::collect_overrides_from_process;
use crate::config::merged::MergeInput;
use crate::config::ResolvedConfig;
use crate::engine::mongo::status::{CheckStatus, StatusChecker, StatusReport};
use crate::error::{Result, XBackupError};

/// backup 자동 사전 점검에서 쓰는 mongodump 실행파일 이름(backup 핸들러와 동일 기본값).
pub const DEFAULT_MONGODUMP: &str = "mongodump";

/// `status` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: StatusArgs) -> Result<()> {
    // 1) config 로드 + 레이어 병합(file + ENV).
    let config_toml = match &config_path {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("config 파일 읽기 실패({}): {e}", path.display()))
        })?),
        None => None,
    };
    let overrides = collect_overrides_from_process();
    let resolved = ResolvedConfig::build(MergeInput {
        config_toml: config_toml.as_deref(),
        profile_name: &args.profile,
        overrides: &overrides,
    })?;

    // 2) URI 시크릿 확보(uri_env 해석값).
    let uri = resolved.resolved_uri.clone().ok_or_else(|| {
        XBackupError::Config(format!(
            "프로파일 '{}'에 source.uri_env가 없거나 해석되지 않았습니다",
            resolved.profile_name
        ))
    })?;

    // 3) 전체 점검 실행(연결 실패도 보고서로 표현 — 여기서는 Err로 일찍 끊지 않는다).
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
            // connect 자체 실패(URI 파싱 등)는 단일 연결 실패 항목 보고서로 만든다.
            Err(e) => StatusReport::new(
                &resolved.profile_name,
                vec![crate::engine::mongo::status::CheckItem::fail(
                    "connection",
                    "연결·인증",
                    format!("연결 준비 실패: {e}"),
                )],
            ),
        };

    // 4) 출력 — --json 구조화 또는 사람용 표.
    if args.json {
        render_json(&report)?;
    } else {
        render_human(&report);
    }

    // 5) 신호등 → 종료 코드. fail이면 PrecheckFailed(exit 3), warn이면 Warning(exit 4), ok면 0.
    report_to_result(&report)
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
}
