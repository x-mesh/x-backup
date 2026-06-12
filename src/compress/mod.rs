//! Compress 계층 — 스트리밍 압축(PRD §FR-6).
//!
//! `Compress` trait: compress_stream / decompress_stream.
//! 구현체: Zstd(async-compression 기반, zstd 멀티스레드 보조).
//!
//! TODO(후속 태스크 R10): Compress trait, zstd 구현.
