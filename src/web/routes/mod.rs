//! 화면 라우트 — 한 화면 = 한 모듈.
//!
//! ## 이 계층의 책임 경계
//! [`crate::web::server`]는 "무엇을 어느 경로에 붙일지"만 알고, 화면의 내용은 이 아래
//! 모듈들이 만든다. 그래서 `server.rs`는 화면이 20개로 늘어도 `.route(...)` 한 줄씩만
//! 늘어난다 — 핸들러 본문이 라우터 조립 코드와 섞이지 않는다.
//!
//! 각 화면 모듈은 다음 세 가지를 노출하는 형태로 맞춘다:
//! 1. 경로 상수(`pub const *_PATH`) — 라우터·마크업·테스트가 문자열을 공유한다.
//! 2. axum 핸들러 하나(`pub async fn page`) — [`crate::web::ServeConfig`]를 state로 받는다.
//! 3. **순수 렌더 함수** — 자식 프로세스나 파일시스템을 건드리지 않고 마크업만 만든다.
//!
//! 3번이 규약인 이유: 이 콘솔의 화면은 대부분 자식 프로세스(`x-backup <cmd> --json`)의
//! 출력을 그리는 일이다. 실행과 렌더가 한 함수에 붙어 있으면 "exit 4를 경고로 그리는가"
//! 같은 판정 로직을 자식을 실제로 띄워야만 테스트할 수 있게 된다. 갈라 두면 판정·렌더는
//! 순수 함수 단위 테스트로, 실행은 별도 테스트로 각각 좁게 검증된다.
//!
//! ## 모듈 구성
//! - [`doctor`]: `GET /doctor` — config 정적 점검 결과.

pub mod backup;
pub mod catalog;
pub mod config;
pub mod dashboard;
pub mod doctor;
pub mod form;
pub mod jobs;
pub mod lock;
pub mod migrate;
pub mod monitor;
pub mod peek;
pub mod prune;
pub mod restore;
pub mod schedule;
pub mod verify;
