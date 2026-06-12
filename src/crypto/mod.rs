//! Crypto 계층 — 스트리밍 암호화(PRD §8).
//!
//! `Crypto` trait: encrypt_stream / decrypt_stream.
//! 구현체: AgeCrypto(기본, X25519 공개키), AesGcmCrypto(대안, 청크 AEAD).
//! 순서 규칙: compress → encrypt 고정.
//!
//! TODO(후속 태스크 R10/R13): Crypto trait, age/aes-gcm 구현.
