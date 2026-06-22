//! 서브커맨드 디스패치와 종료 코드 연결.
//!
//! 모든 서브커맨드가 실제 핸들러로 라우팅된다. 각 핸들러는 [`crate::error::Result`]를
//! 반환하고, `main`이 에러의 [`crate::error::XBackupError::exit_code`]로 프로세스를 종료한다.

use std::io::IsTerminal;

use crate::cli::{handlers, Cli, Command};
use crate::error::Result;

/// 파싱된 CLI를 해당 핸들러로 디스패치한다.
///
/// 모든 서브커맨드가 실제 핸들러로 라우팅된다(스텁 없음). 각 핸들러는
/// [`Result`]를 반환하고 `main`이 에러의 exit code로 프로세스를 종료한다.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let Cli {
        config,
        lang,
        command,
        ..
    } = cli;
    // --config/XB_CONFIG 미지정이면 프로젝트 로컬 표준 위치를 탐색한다(매번 풀패스 입력 완화).
    let config = crate::cli::resolve_config_path(config);
    // config를 쓰는 서브커맨드인데 끝내 못 찾았으면, 대화형에서만 한 줄 안내한다(범용 cryptic
    // 에러를 보기 전에 길을 알려줌). env-only(XB_SOURCE__URI 등) 주입은 유효하므로 차단하지
    // 않고, 비대화형(cron/CI)은 노이즈 방지를 위해 침묵한다. init/update는 config가 불필요.
    let needs_config = !matches!(command, Command::Init(_) | Command::Update(_));
    if needs_config && config.is_none() && std::io::stderr().is_terminal() {
        eprintln!(
            "ℹ config 미지정 — `--config <PATH>`/`XB_CONFIG`로 지정하거나, 현재 디렉터리에 \
             `xbackup.toml`을 두면 자동 인식합니다. xbenv 워크스페이스면 `source <ws>/activate`, \
             env로 직접 주입하려면 `XB_SOURCE__URI` 등을 설정하세요."
        );
    }
    match command {
        Command::Init(args) => handlers::init::handle(config, lang, args).await,
        Command::Backup(args) => handlers::backup::handle(config, lang, args).await,
        Command::Restore(args) => handlers::restore::handle(config, lang, args).await,
        Command::List(args) => handlers::list::handle(config, lang, args).await,
        Command::Verify(args) => handlers::verify::handle(config, lang, args).await,
        Command::Prune(args) => handlers::prune::handle(config, lang, args).await,
        Command::Status(args) => handlers::status::handle(config, lang, args).await,
        Command::Doctor(args) => handlers::doctor::handle(config, lang, args).await,
        Command::Peek(args) => handlers::peek::handle(config, lang, args).await,
        Command::Migrate(args) => handlers::migrate::handle(config, lang, args).await,
        Command::Update(args) => handlers::update::handle(lang, args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::args::{InitArgs, StatusArgs};

    /// 테스트용 Cli 래퍼(global 플래그 기본값 + 주어진 서브커맨드).
    fn cli_with(command: Command) -> Cli {
        Cli {
            config: None,
            lang: None,
            verbose: 0,
            command,
        }
    }

    /// init은 라우팅된다 — 테스트 환경(비-TTY)에서는 대화형 가드로 Usage(exit 2)다.
    /// (더 이상 미구현 스텁 exit 1이 아니다.)
    #[tokio::test]
    async fn init_handler_routes_and_guards_non_tty() {
        let result = dispatch(cli_with(Command::Init(InitArgs { force: false }))).await;
        let err = result.expect_err("비-TTY에서는 마법사 가드로 에러여야 함");
        assert_eq!(err.exit_code(), 2);
    }

    /// status 핸들러로 라우팅된다. config·URI가 없는 프로파일이면 설정 오류(exit 2)로
    /// 끊긴다(연결 시도 전에 uri_env 부재를 잡는다 — 무부작용).
    #[tokio::test]
    async fn status_handler_routes_through_exit_code() {
        let result = dispatch(cli_with(Command::Status(StatusArgs {
            profile: Some("prod".into()),
            all: false,
            json: false,
            watch: false,
            interval: 1.0,
            count: 0,
            ns_detail: false,
        })))
        .await;
        // uri_env가 없는 프로파일 → Config(exit 2). 더 이상 미구현 스텁(exit 1)이 아니다.
        assert_eq!(result.unwrap_err().exit_code(), 2);
    }
}
