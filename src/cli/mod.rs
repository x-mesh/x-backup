//! CLI 계층 — clap derive 기반 인자 파싱, 출력 모드, 종료 코드 처리.
//!
//! 모듈 구성:
//! - [`args`]: PRD §9 서브커맨드 트리(clap derive).
//! - [`output`]: 출력 모드(progress/quiet/json) 결정 규칙(R15).
//! - [`picker`]: 인터랙티브 백업 선택(restore --id 미지정 + TTY) — fuzzy finder.
//! - [`progress`]: 진행 표시(indicatif, stderr 타깃) — R16.
//! - [`exit`]: 핸들러 디스패치와 종료 코드 연결.
//! - [`table`]: 표시 폭(한글/CJK) 인지 정렬 표 + ANSI 색 유틸(status·migrate 공용).
//!
//! 출력 설명 언어(ko/en) 선택은 크레이트 루트 [`crate::i18n`]에 있다(engine 레이어도
//! 참조하므로 cli 하위가 아니라 루트에 둔다). 라벨·기술용어는 항상 영문이다.

pub mod args;
pub mod exit;
pub mod handlers;
pub mod output;
pub mod picker;
pub mod progress;
pub mod table;

pub use crate::i18n::Lang;
pub use args::{Cli, Command};

/// 명시적 config(`--config`/`XB_CONFIG`)가 없을 때만 프로젝트 로컬 표준 위치를 탐색한다.
///
/// x-backup은 의도적으로 광범위한 자동 탐색을 하지 않는다(어느 config를 보는지 모호해지지
/// 않게 — FR-10). 다만 현재 디렉터리의 표준 파일 하나 정도는 매번 `--config` 풀패스를 적는
/// 마찰을 줄여준다. 발견 경로는 핸들러의 `config:` 라인으로 그대로 표면화되어 "지금 무엇을
/// 보는지"가 가려지지 않는다. clap이 `XB_CONFIG`를 `--config`로 접어주므로 여기서 `explicit`이
/// `Some`이면 둘 중 하나가 지정된 것 — 그땐 탐색하지 않는다.
pub fn resolve_config_path(explicit: Option<std::path::PathBuf>) -> Option<std::path::PathBuf> {
    if explicit.is_some() {
        return explicit;
    }
    const CANDIDATES: [&str; 2] = ["xbackup.toml", ".xbackup/config.toml"];
    CANDIDATES
        .iter()
        .map(std::path::PathBuf::from)
        .find(|p| p.is_file())
}
