//! x-backup 바이너리 진입점.
//!
//! 책임은 얇게 유지한다: 인자 파싱 → tracing 초기화 → 디스패치 → exit code 변환.
//! 종료 코드의 단일 진실 공급원은 [`x_backup::XBackupError::exit_code`]다(PRD §9).

use std::process::ExitCode;

use clap::Parser;

use x_backup::cli::{exit, Cli};
use x_backup::init_tracing;

#[tokio::main]
async fn main() -> ExitCode {
    // clap 파싱 실패(잘못된 플래그 등)는 clap이 사용법을 출력하고 exit 2로 종료한다.
    let cli = Cli::parse();

    // tracing 초기화. --json 결정은 후속 태스크에서 서브커맨드 플래그와 연동하나,
    // 스캐폴드 단계에서는 사람이 읽는 형식 + verbosity만 사용한다.
    init_tracing(cli.verbose, false);

    match exit::dispatch(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // 시크릿이 새지 않도록 에러는 Display로만 출력한다(Secret은 [REDACTED]).
            tracing::error!("{err}");
            err.exit()
        }
    }
}
