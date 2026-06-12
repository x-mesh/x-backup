//! `init` 서브커맨드 핸들러 — 대화형 마법사로 config.toml 생성(PRD §FR-10, R18).
//!
//! 흐름: 비-TTY 가드 → 기존 파일 가드(--force) → 마법사 실행 → config.toml 기록 →
//! (선택) status 즉시 검증.
//!
//! ## 가드레일
//! - **대화형 전용:** stdin이 TTY가 아니면 거부([`XBackupError::Usage`], exit 2) —
//!   마법사는 질문/응답이 전제다(비대화형은 EOF로 끊겨 부분 config가 생길 수 있음).
//! - **기존 파일 보호:** 대상 config.toml이 이미 있으면 `--force` 없이는 거부(exit 2) —
//!   기존 설정을 실수로 덮어쓰지 않게(FR-10).
//! - **시크릿 비저장:** 마법사가 env 참조만 받으므로 기록물에 평문 시크릿이 없다.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use crate::cli::args::{InitArgs, StatusArgs};
use crate::config::wizard::{config_to_toml, run_wizard, Prompt, StdinPrompt};
use crate::config::Config;
use crate::error::{Result, XBackupError};

/// init이 config 경로 미지정 시 사용할 기본 파일명(현재 디렉터리 기준).
const DEFAULT_CONFIG_FILENAME: &str = "config.toml";

/// `init` 핸들러 진입점.
pub async fn handle(config_path: Option<PathBuf>, args: InitArgs) -> Result<()> {
    // 1) 비-TTY 가드 — 마법사는 대화형 전용. 표준입력이 터미널이 아니면 거부한다.
    if !std::io::stdin().is_terminal() {
        return Err(XBackupError::Usage(
            "init 마법사는 대화형(TTY) 전용입니다 — 파이프/CI에서는 config.toml을 직접 \
             작성하거나 ENV(XB_*)로 구성하세요"
                .into(),
        ));
    }

    // 2) 대상 경로 결정 + 기존 파일 가드(--force).
    let target = config_path.unwrap_or_else(|| PathBuf::from(DEFAULT_CONFIG_FILENAME));
    guard_existing(&target, args.force)?;

    // 3) 마법사 실행(실 stdin) → Config.
    let mut prompt = StdinPrompt;
    let config = run_wizard(&mut prompt)?;

    // 4) config.toml 기록.
    write_config(&target, &config)?;
    // 안내는 stderr로(결과 채널 분리). 경로를 stdout으로 한 줄 알린다(스크립트 활용).
    eprintln!("config.toml을 생성했습니다: {}", target.display());
    println!("{}", target.display());

    // 5) (선택) status 즉시 검증 — 동의 시 status 핸들러를 호출한다(FR-10 "생성 후 status").
    if let Some(profile_name) = config.default_profile.clone() {
        if ask_run_status(&mut prompt)? {
            // status 핸들러는 같은 config 경로를 읽어 연결·권한을 점검한다.
            // 경고/실패도 정상 종료 코드로 표현되므로 여기서 에러를 흡수하지 않고 전파한다.
            let status_args = StatusArgs {
                profile: profile_name,
                json: false,
            };
            return crate::cli::handlers::status::handle(Some(target), status_args).await;
        }
    }

    Ok(())
}

/// 마지막 질문 — 지금 status로 연결·권한을 검증할지.
fn ask_run_status(prompt: &mut dyn Prompt) -> Result<bool> {
    let answer = prompt.read_line("지금 status로 연결·권한을 검증하시겠습니까? [y/N]: ")?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

/// 기존 config 파일 가드 — 존재하는데 `--force`가 없으면 거부한다(exit 2).
fn guard_existing(target: &Path, force: bool) -> Result<()> {
    if target.exists() && !force {
        return Err(XBackupError::Usage(format!(
            "config 파일이 이미 존재합니다: {} — 덮어쓰려면 --force를 지정하세요",
            target.display()
        )));
    }
    Ok(())
}

/// Config를 TOML로 직렬화해 파일에 기록한다(상위 디렉터리는 필요 시 생성).
fn write_config(target: &Path, config: &Config) -> Result<()> {
    let toml = config_to_toml(config)?;
    if let Some(parent) = target.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent).map_err(|e| {
                XBackupError::Failure(format!(
                    "config 디렉터리 생성 실패({}): {e}",
                    parent.display()
                ))
            })?;
        }
    }
    std::fs::write(target, toml).map_err(|e| {
        XBackupError::Failure(format!("config.toml 기록 실패({}): {e}", target.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 기존 파일 + --force 없음 → 거부(exit 2).
    #[test]
    fn guard_rejects_existing_without_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        let err = guard_existing(&path, false).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    /// 기존 파일 + --force → 통과.
    #[test]
    fn guard_allows_existing_with_force() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "x").unwrap();
        assert!(guard_existing(&path, true).is_ok());
    }

    /// 없는 파일 → 항상 통과.
    #[test]
    fn guard_allows_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nope.toml");
        assert!(guard_existing(&path, false).is_ok());
    }

    /// write_config가 시크릿 평문 없는 파일을 만들고, 다시 파싱 가능하다.
    #[test]
    fn write_config_roundtrips_without_secrets() {
        use crate::config::wizard::{run_wizard, VecPrompt};
        let mut prompt = VecPrompt::new(vec![
            "prod",
            "MONGO_URI",
            "n",
            "local",
            "/data/backups",
            "10",
            "n",
            "15m",
            "progress",
        ]);
        let cfg = run_wizard(&mut prompt).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("config.toml");
        write_config(&path, &cfg).unwrap();

        let written = std::fs::read_to_string(&path).unwrap();
        assert!(written.contains("MONGO_URI"));
        assert!(!written.contains("mongodb://"));
        // 다시 파싱되어야 한다(왕복 안전).
        let reparsed = Config::from_toml_str(&written).unwrap();
        assert_eq!(reparsed.default_profile.as_deref(), Some("prod"));
        assert_eq!(
            reparsed
                .profile("prod")
                .unwrap()
                .destination
                .path
                .as_deref(),
            Some("/data/backups")
        );
    }
}
