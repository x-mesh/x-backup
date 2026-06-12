//! CLI 계층 — clap derive 기반 인자 파싱, 출력 모드, 종료 코드 처리.
//!
//! 모듈 구성:
//! - [`args`]: PRD §9 서브커맨드 트리(clap derive).
//! - [`output`]: 출력 모드(progress/quiet/json) 결정 규칙(R15).
//! - [`progress`]: 진행 표시(indicatif, stderr 타깃) — R16.
//! - [`exit`]: 핸들러 디스패치와 종료 코드 연결.

pub mod args;
pub mod exit;
pub mod handlers;
pub mod output;
pub mod progress;

pub use args::{Cli, Command};
