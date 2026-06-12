//! Lock 계층 — 동시 실행 잠금(PRD §FR-12).
//!
//! 로컬 lock 파일(1차 결정): LockData{pid, started_at, profile, hostname},
//! O_CREAT|O_EXCL 원자 생성, stale = PID 부재 또는 연령 초과, Drop 시 제거.
//! 잠금 충돌 시 exit 5(LockConflict).
//!
//! TODO(후속 태스크 R20): file_lock, stale 감지·해제 구현.
