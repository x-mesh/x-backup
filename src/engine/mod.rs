//! Engine 계층 — DB별 백업/복구 어댑터(PRD §10).
//!
//! 1차는 [`mongo`] 어댑터만 구현한다. `Engine` trait 추상화는 2차 PostgreSQL을
//! 무변경으로 얹기 위한 것이나, 1차 수직 슬라이스에서는 MongoDB 구체 타입을 직접
//! 사용한다(과도한 선추상화 회피) — trait 일반화는 2차 착수 시 도입한다.
//!
//! TODO(후속 태스크 t5/t8/t14): restore(mongorestore)·증분 oplog 캡처·status 점검.

pub mod mongo;
