//! Lock 계층 — 동시 실행 잠금(PRD §FR-12).
//!
//! 로컬 lock 파일(1차 결정): [`LockData`]{pid, started_at, profile, hostname},
//! `O_CREAT | O_EXCL` 원자 생성, stale = PID 부재(자동 회수) 또는 연령 초과(안내),
//! [`LockGuard`] Drop 시 제거. 잠금 충돌 시 exit 5([`crate::error::XBackupError::LockConflict`]).
//!
//! 진입점은 [`acquire`](file_lock::acquire) — 프로파일명으로 lock을 잡고
//! [`LockGuard`]를 돌려준다. backup/restore/prune 핸들러가 작업 시작부에서 호출하고
//! 가드를 작업 동안 변수로 유지한다(읽기 전용인 status/list/verify는 잠그지 않는다).

pub mod file_lock;

pub use file_lock::{acquire, acquire_in, LockData, LockGuard};
