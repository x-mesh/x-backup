//! 서브커맨드 디스패치와 종료 코드 연결.
//!
//! 핸들러는 현재 "not implemented" 스텁이지만, 종료 코드 경로는 완전히 동작한다.
//! 각 핸들러는 [`crate::error::Result`]를 반환하고, `main`이 에러의
//! [`crate::error::XBackupError::exit_code`]로 프로세스를 종료한다.

use crate::cli::{handlers, Cli, Command};
use crate::error::{Result, XBackupError};

/// 파싱된 CLI를 해당 핸들러로 디스패치한다.
///
/// `backup`(t4)·`restore`(t5)는 실제 파이프라인을 실행한다. 나머지는 후속 태스크가
/// 구현할 때까지 미구현 스텁([`XBackupError::Failure`], exit 1)으로 둔다.
pub async fn dispatch(cli: Cli) -> Result<()> {
    let Cli {
        config, command, ..
    } = cli;
    match command {
        Command::Init(_) => not_implemented("init"),
        Command::Backup(args) => handlers::backup::handle(config, args).await,
        Command::Restore(args) => handlers::restore::handle(config, args).await,
        Command::List(args) => handlers::list::handle(config, args).await,
        Command::Verify(args) => handlers::verify::handle(config, args).await,
        Command::Prune(_) => not_implemented("prune"),
        Command::Status(args) => handlers::status::handle(config, args).await,
    }
}

/// 미구현 핸들러용 공통 에러. 종료 코드 경로 검증을 위해 실패(exit 1)로 반환한다.
fn not_implemented(name: &str) -> Result<()> {
    Err(XBackupError::Failure(format!(
        "'{name}' 서브커맨드는 아직 구현되지 않았습니다"
    )))
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

    /// 미구현 핸들러는 exit 1(Failure)로 반환된다.
    #[tokio::test]
    async fn unimplemented_handler_returns_failure() {
        let result = dispatch(cli_with(Command::Init(InitArgs { force: false }))).await;
        let err = result.expect_err("미구현이므로 에러여야 함");
        assert_eq!(err.exit_code(), 1);
    }

    /// status 핸들러로 라우팅된다. config·URI가 없는 프로파일이면 설정 오류(exit 2)로
    /// 끊긴다(연결 시도 전에 uri_env 부재를 잡는다 — 무부작용).
    #[tokio::test]
    async fn status_handler_routes_through_exit_code() {
        let result = dispatch(cli_with(Command::Status(StatusArgs {
            profile: "prod".into(),
            json: false,
        })))
        .await;
        // uri_env가 없는 프로파일 → Config(exit 2). 더 이상 미구현 스텁(exit 1)이 아니다.
        assert_eq!(result.unwrap_err().exit_code(), 2);
    }
}
