//! 설정 계층 — config.toml + 프로파일 + ENV 오버라이드 + 시크릿 env 참조.
//!
//! 우선순위(높음→낮음): **CLI 플래그 > 환경변수(ENV) > config.toml > 내장 기본값** (PRD §FR-10).
//!
//! 모듈 구성:
//! - [`file`]: config.toml의 serde 구조와 파싱.
//! - [`env`]: `XB_` 접두사 + `__` 구분자 평탄화 오버라이드, 시크릿 env 참조 해석.
//! - [`merged`]: 레이어를 합쳐 최종 [`merged::ResolvedConfig`]를 만든다.
//! - [`secret`]: Debug 출력을 억제하는 시크릿 래퍼 타입.

pub mod env;
pub mod file;
pub mod merged;
pub mod secret;
pub mod v2;
pub mod wizard;

pub use file::{Config, Profile};
pub use merged::ResolvedConfig;
pub use secret::Secret;
