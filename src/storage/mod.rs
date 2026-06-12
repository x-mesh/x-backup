//! Storage 계층 — 추상 스토리지 백엔드(PRD §10).
//!
//! `Storage` trait: put_stream / get_stream / list / delete.
//! 구현체: LocalFs, S3Compatible(object_store v0.13 기반).
//!
//! TODO(후속 태스크 R8/R9): Storage trait, LocalFs, S3 멀티파트+abort 구현.
