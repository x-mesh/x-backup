//! 서브커맨드 디스패치와 종료 코드 연결.
//!
//! 핸들러는 현재 "not implemented" 스텁이지만, 종료 코드 경로는 완전히 동작한다.
//! 각 핸들러는 [`crate::error::Result`]를 반환하고, `main`이 에러의
//! [`crate::error::XBackupError::exit_code`]로 프로세스를 종료한다.

use crate::cli::Command;
use crate::error::{Result, XBackupError};

/// 파싱된 서브커맨드를 해당 핸들러로 디스패치한다.
///
/// 모든 핸들러가 미구현이므로 현재는 [`XBackupError::Failure`](exit 1)를 반환한다.
/// 후속 태스크가 각 핸들러를 실제 로직으로 대체한다.
pub async fn dispatch(command: Command) -> Result<()> {
    match command {
        Command::Init(_) => not_implemented("init"),
        Command::Backup(_) => not_implemented("backup"),
        Command::Restore(_) => not_implemented("restore"),
        Command::List(_) => not_implemented("list"),
        Command::Verify(_) => not_implemented("verify"),
        Command::Prune(_) => not_implemented("prune"),
        Command::Status(_) => not_implemented("status"),
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

    /// 미구현 핸들러는 exit 1(Failure)로 반환된다.
    #[tokio::test]
    async fn unimplemented_handler_returns_failure() {
        let result = dispatch(Command::Init(InitArgs { force: false })).await;
        let err = result.expect_err("미구현이므로 에러여야 함");
        assert_eq!(err.exit_code(), 1);
    }

    /// status 핸들러도 동일하게 종료 코드 경로를 탄다.
    #[tokio::test]
    async fn status_handler_routes_through_exit_code() {
        let result = dispatch(Command::Status(StatusArgs {
            profile: "prod".into(),
            json: false,
        }))
        .await;
        assert_eq!(result.unwrap_err().exit_code(), 1);
    }
}
