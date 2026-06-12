//! 출력 모드 결정 — progress / quiet / json (PRD §FR-9, R15/R16).
//!
//! 스텁: 우선순위(CLI 플래그 > config 기본값 > TTY 자동 감지) 규칙의 골격만 둔다.
//! 실제 progress 렌더링(indicatif 등)과 JSON 라인 출력은 후속 태스크가 구현한다.

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

impl OutputMode {
    /// CLI 플래그와 TTY 여부로 출력 모드를 결정한다.
    ///
    /// 우선순위: `--json` > `--quiet`/`--progress` > 비-TTY 자동 quiet > progress 기본.
    /// config 기본값 머지는 후속 태스크에서 이 함수 앞단에 끼운다.
    pub fn resolve(json: bool, quiet: bool, progress: bool, is_tty: bool) -> Self {
        if json {
            Self::Json
        } else if quiet {
            Self::Quiet
        } else if progress || is_tty {
            // --progress 강제 또는 TTY 기본 — 둘 다 진행 표시.
            Self::Progress
        } else {
            // 비-TTY(파이프/cron)에서는 진행 바가 로그를 오염시키지 않도록 자동 quiet.
            Self::Quiet
        }
    }

    /// 현재 stdout의 TTY 여부를 사용해 출력 모드를 결정한다.
    pub fn resolve_for_stdout(json: bool, quiet: bool, progress: bool) -> Self {
        Self::resolve(json, quiet, progress, std::io::stdout().is_terminal())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_flag_wins() {
        assert_eq!(
            OutputMode::resolve(true, true, true, true),
            OutputMode::Json
        );
    }

    #[test]
    fn non_tty_defaults_to_quiet() {
        assert_eq!(
            OutputMode::resolve(false, false, false, false),
            OutputMode::Quiet
        );
    }

    #[test]
    fn tty_defaults_to_progress() {
        assert_eq!(
            OutputMode::resolve(false, false, false, true),
            OutputMode::Progress
        );
    }

    #[test]
    fn progress_forces_on_non_tty() {
        assert_eq!(
            OutputMode::resolve(false, false, true, false),
            OutputMode::Progress
        );
    }
}
