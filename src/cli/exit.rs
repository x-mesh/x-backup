//! 서브커맨드 디스패치와 종료 코드 연결.
//!
//! 모든 서브커맨드가 실제 핸들러로 라우팅된다. 각 핸들러는 [`crate::error::Result`]를
//! 반환하고, `main`이 에러의 [`crate::error::XBackupError::exit_code`]로 프로세스를 종료한다.

use crate::cli::{handlers, Cli, Command};
use crate::error::Result;

/// 파싱된 CLI를 해당 핸들러로 디스패치한다.
///
/// 모든 서브커맨드가 실제 핸들러로 라우팅된다(스텁 없음). 각 핸들러는
/// [`Result`]를 반환하고 `main`이 에러의 exit code로 프로세스를 종료한다.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let Cli {
        config, command, ..
    } = cli;
    match command {
        Command::Init(args) => handlers::init::handle(config, args).await,
        Command::Backup(args) => handlers::backup::handle(config, args).await,
        Command::Restore(args) => handlers::restore::handle(config, args).await,
        Command::List(args) => handlers::list::handle(config, args).await,
        Command::Verify(args) => handlers::verify::handle(config, args).await,
        Command::Prune(args) => handlers::prune::handle(config, args).await,
        Command::Status(args) => handlers::status::handle(config, args).await,
        Command::Peek(args) => handlers::peek::handle(config, args).await,
        Command::Migrate(args) => handlers::migrate::handle(config, args).await,
        Command::Update(args) => handlers::update::handle(args).await,
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
        })))
        .await;
        // uri_env가 없는 프로파일 → Config(exit 2). 더 이상 미구현 스텁(exit 1)이 아니다.
        assert_eq!(result.unwrap_err().exit_code(), 2);
    }
}
