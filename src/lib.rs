//! x-backup — MongoDB 백업·복구 CLI 라이브러리 크레이트.
//!
//! 바이너리(`main.rs`)는 이 크레이트의 얇은 래퍼다. 로직은 모듈에 두어
//! 단위·통합 테스트에서 직접 호출할 수 있게 한다.
//!
//! 모듈 레이아웃은 리서치 architecture 설계를 따른다(PRD §10 trait 경계):
//! - [`cli`]: 인자 파싱·출력 모드·디스패치.
//! - [`config`]: config.toml + ENV + 시크릿 레이어링.
//! - [`error`]: 에러 모델과 exit code 매핑.
//! - [`engine`]/[`storage`]/[`crypto`]/[`compress`]/[`pipeline`]/[`manifest`]/[`lock`]:
//!   도메인 계층(스캐폴드 단계에서는 스텁).

pub mod cli;
pub mod compress;
pub mod config;
pub mod crypto;
pub mod engine;
pub mod error;
pub mod i18n;
pub mod lock;
pub mod manifest;
pub mod pipeline;
pub mod storage;
pub mod update;

pub use error::{Result, XBackupError};

/// `tracing` 구독자를 초기화한다(PRD §11 구조화 로그).
///
/// - `verbosity`: CLI `-v` 횟수(0=warn, 1=info, 2=debug, 3+=trace).
/// - `json`: true면 JSON 라인 로그(모니터링 연동), false면 사람이 읽는 형식.
///
/// `RUST_LOG` 환경변수가 있으면 그 필터가 우선한다(12-factor).
/// 이미 전역 구독자가 설치된 경우(테스트 등)에는 조용히 무시한다.
pub fn init_tracing(verbosity: u8, json: bool) {
    use std::io::IsTerminal;
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    // verbosity → 필터. -vv부터는 MongoDB 드라이버 로그도 끌어올려(연결·토폴로지 진단)
    // "더 많은 출력"을 준다. 0=warn / 1=info / 2=debug(+driver info) / 3+=trace(+driver debug).
    let default_filter = match verbosity {
        0 => "x_backup=warn".to_string(),
        1 => "x_backup=info".to_string(),
        2 => "x_backup=debug,mongodb=info".to_string(),
        _ => "x_backup=trace,mongodb=debug,object_store=debug".to_string(),
    };

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));

    // -vv 이상에서는 사람용 포맷에 target·라인 번호를 붙여 어디서 나온 로그인지 보이게 한다.
    // -vvv(trace)에서는 스레드 ID까지(동시 파이프라인 추적용).
    let show_loc = verbosity >= 2;
    let show_thread = verbosity >= 3;
    let ansi = std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal();

    // 로그는 stderr로 보낸다 — stdout은 결과·progress·--json 출력 전용(PRD §9-9.1).
    let registry = tracing_subscriber::registry().with(filter);
    let installed = if json {
        registry
            .with(fmt::layer().json().with_writer(std::io::stderr))
            .try_init()
    } else {
        registry
            .with(
                fmt::layer()
                    .with_writer(std::io::stderr)
                    .with_ansi(ansi)
                    .with_target(show_loc)
                    .with_line_number(show_loc)
                    .with_thread_ids(show_thread),
            )
            .try_init()
    };
    // 중복 초기화 에러는 무시한다(테스트에서 여러 번 호출될 수 있음).
    let _ = installed;
}
