//! 출력 모드 결정 — progress / quiet / json (PRD §FR-9, R15/R16).
//!
//! 우선순위(높음→낮음, PRD §FR-9/§FR-10):
//! **CLI(`--json` > `--quiet` > `--progress`) > config `mode.output` > TTY 자동 감지.**
//!
//! ## 핵심 규약(pitfall 9-1)
//! - **progress·로그는 stderr**, **결과·`--json`은 stdout**으로 분리한다. 따라서 TTY
//!   자동 감지는 *stderr*가 터미널인지로 판단한다(stdout이 파이프로 리다이렉트돼도
//!   사람이 보는 stderr가 터미널이면 진행 표시가 유효하기 때문).
//! - 비-TTY(cron/CI/파이프)에서는 자동으로 quiet 동작 — 진행 바가 로그를 오염시키지
//!   않도록. `--progress`로 강제할 수 있다.

use std::io::IsTerminal;

/// 사용자에게 보여줄 출력 모드.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// 단계별 진행 표시(TTY 기본).
    Progress,
    /// 진행 억제, 요약·경고·에러만(cron/CI, 비-TTY 자동).
    Quiet,
    /// 기계 판독 JSON.
    Json,
}

/// 출력 모드 결정에 쓰는 CLI 플래그 묶음.
///
/// backup/restore가 공유하는 `--json`/`--quiet`/`--progress`를 한곳에 모은다(clap에서
/// 이미 `--quiet`↔`--progress` 상호 배타가 보장되므로 둘이 동시에 true일 일은 없다).
#[derive(Debug, Clone, Copy, Default)]
pub struct OutputFlags {
    /// `--json` — 진행/결과를 기계 판독 JSON으로.
    pub json: bool,
    /// `--quiet` — 진행 억제.
    pub quiet: bool,
    /// `--progress` — 비-TTY에서도 진행 강제.
    pub progress: bool,
}

impl OutputMode {
    /// CLI 플래그·config 기본값·TTY 여부로 출력 모드를 결정한다(전체 규칙).
    ///
    /// 우선순위: `--json` > `--quiet` > `--progress` > config(`mode.output`) > TTY 자동.
    /// `config_output`은 config `mode.output`("progress"|"quiet"); 알 수 없는 값은
    /// 무시하고 TTY 자동 감지로 폴백한다(설정 결함이 출력에 치명적이지 않도록).
    pub fn resolve(flags: OutputFlags, config_output: Option<&str>, is_tty: bool) -> Self {
        if flags.json {
            return Self::Json;
        }
        if flags.quiet {
            return Self::Quiet;
        }
        if flags.progress {
            // --progress는 비-TTY에서도 진행을 강제한다.
            return Self::Progress;
        }
        // CLI에서 출력 모드를 지정하지 않았으면 config 기본값을 본다.
        match config_output {
            Some("quiet") => Self::Quiet,
            Some("progress") => {
                // config가 progress를 원해도, 비-TTY면 진행 바가 로그를 오염시키므로
                // 자동 quiet로 낮춘다(명시적 --progress만 비-TTY 강제).
                if is_tty {
                    Self::Progress
                } else {
                    Self::Quiet
                }
            }
            // config 미지정/알 수 없는 값 → TTY 자동 감지.
            _ => {
                if is_tty {
                    Self::Progress
                } else {
                    Self::Quiet
                }
            }
        }
    }

    /// 현재 프로세스의 **stderr** TTY 여부로 출력 모드를 결정한다(실사용 진입점).
    ///
    /// progress·로그는 stderr로 나가므로(pitfall 9-1), 진행 표시 가능 여부는 stderr가
    /// 터미널인지로 판단한다 — stdout이 파이프로 묶여도 사람이 보는 stderr 기준.
    pub fn resolve_from_env(flags: OutputFlags, config_output: Option<&str>) -> Self {
        Self::resolve(flags, config_output, std::io::stderr().is_terminal())
    }

    /// 진행 표시(progress bar/spinner)를 그려야 하는 모드인지.
    ///
    /// `Progress`만 true다. `Quiet`/`Json`은 진행 바를 그리지 않는다(Json은 결과만,
    /// 진행 *이벤트*는 별도로 stderr JSON 라인으로 선택 출력 — [`Self::emits_json`] 참조).
    pub fn shows_progress_bar(self) -> bool {
        matches!(self, Self::Progress)
    }

    /// 사람용 요약(stdout 텍스트)을 출력하는 모드인지. `Json`/`Quiet`은 false.
    pub fn shows_human_summary(self) -> bool {
        matches!(self, Self::Progress)
    }

    /// 결과를 JSON으로 출력하는 모드인지(`--json`).
    pub fn emits_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

/// `--json`만 받는 핸들러(list/status/peek/prune/pitr)에서 컨텍스트 표시 모드를 만든다 —
/// json이면 Json(컨텍스트 생략), 아니면 stderr TTY 여부로 Progress/Quiet.
pub fn context_mode(json: bool) -> OutputMode {
    OutputMode::resolve_from_env(
        OutputFlags {
            json,
            quiet: false,
            progress: false,
        },
        None,
    )
}

/// 실행 컨텍스트 한 줄(어떤 프로파일·어떤 DB)을 **stderr**에 표시한다.
///
/// x-backup은 MongoDB·PostgreSQL을 한 바이너리로 다루는 다중 DB 툴이라, 모든 명령이 "지금
/// 어느 프로파일·어느 DB를 건드리는지"를 항상 보여주는 게 안전하다. 결과(stdout)·`--json`을
/// 오염시키지 않도록 stderr로 내보내고, 사람용(Progress) 모드에서만 표시한다(quiet/json 생략).
pub fn print_run_context(profile: &str, db: Option<crate::engine::DbKind>, mode: OutputMode) {
    // json만 제외(기계 판독 오염 방지). progress·quiet 모두 stderr에 한 줄 — 다중 DB 툴에서
    // "지금 무엇을 건드리는지"는 quiet에서도 보이는 게 안전하다.
    if mode.emits_json() {
        return;
    }
    use crate::cli::table::{paint, use_color, DIM};
    let line = match db {
        Some(d) => format!("▸ 프로파일 {profile} · DB {}", d.label()),
        None => format!("▸ 프로파일 {profile}"),
    };
    eprintln!("{}", paint(&line, &[DIM], use_color()));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flags(json: bool, quiet: bool, progress: bool) -> OutputFlags {
        OutputFlags {
            json,
            quiet,
            progress,
        }
    }

    #[test]
    fn json_flag_wins_over_everything() {
        // json + quiet + progress + config quiet + tty → Json.
        assert_eq!(
            OutputMode::resolve(flags(true, true, true), Some("quiet"), true),
            OutputMode::Json
        );
    }

    #[test]
    fn quiet_flag_beats_config_and_tty() {
        assert_eq!(
            OutputMode::resolve(flags(false, true, false), Some("progress"), true),
            OutputMode::Quiet
        );
    }

    #[test]
    fn progress_flag_forces_on_non_tty() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, true), Some("quiet"), false),
            OutputMode::Progress
        );
    }

    #[test]
    fn config_quiet_applies_when_no_cli_flag() {
        // CLI 미지정 + config quiet → Quiet(TTY여도).
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("quiet"), true),
            OutputMode::Quiet
        );
    }

    #[test]
    fn config_progress_on_tty_is_progress() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("progress"), true),
            OutputMode::Progress
        );
    }

    #[test]
    fn config_progress_on_non_tty_falls_back_to_quiet() {
        // config가 progress를 원해도 비-TTY면 자동 quiet(명시 --progress만 강제).
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("progress"), false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn no_config_tty_defaults_to_progress() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), None, true),
            OutputMode::Progress
        );
    }

    #[test]
    fn no_config_non_tty_defaults_to_quiet() {
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), None, false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn unknown_config_value_falls_back_to_tty_auto() {
        // 알 수 없는 config 값은 무시하고 TTY 자동 감지.
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("garbage"), true),
            OutputMode::Progress
        );
        assert_eq!(
            OutputMode::resolve(flags(false, false, false), Some("garbage"), false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn mode_predicates() {
        assert!(OutputMode::Progress.shows_progress_bar());
        assert!(!OutputMode::Quiet.shows_progress_bar());
        assert!(!OutputMode::Json.shows_progress_bar());

        assert!(OutputMode::Progress.shows_human_summary());
        assert!(!OutputMode::Quiet.shows_human_summary());
        assert!(!OutputMode::Json.shows_human_summary());

        assert!(OutputMode::Json.emits_json());
        assert!(!OutputMode::Progress.emits_json());
    }
}
