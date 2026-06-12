//! Engine 계층 — DB별 백업/복구 어댑터(PRD §10).
//!
//! `Engine` trait: status / backup_full / backup_incr / restore.
//! 1차는 MongoAdapter만 구현하며, 2차 PostgreSQL을 무변경으로 얹기 위해 trait를 미리 분리한다.
//!
//! TODO(후속 태스크 R1/R3/R5/R6/R14): MongoAdapter, async_trait Engine, oplog 리더 구현.
