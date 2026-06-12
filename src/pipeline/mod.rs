//! Pipeline 계층 — 백업/복구 스트리밍 파이프 합성(PRD §7).
//!
//! 백업: mongodump stdout → compress → encrypt → [sha256 tee] → put_stream → manifest.
//! 복구: get_stream → decrypt → decompress → mongorestore stdin.
//! 체크섬 기준점 = 암호화 후 저장 바이트(키 없이 구조 검증 가능).
//!
//! TODO(후속 태스크 R1/R5): backup/restore 파이프, sha256 tee AsyncRead 구현.
