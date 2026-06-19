//! AES-256-GCM + STREAM 청크 AEAD 암호화 단계 — 대칭 대안 경로(PRD §8.2).
//!
//! 키 공유가 단순한 폐쇄 환경용 대안이다. 단일 GCM nonce로 대용량을 처리하면 nonce
//! 재사용·2^32 블록 wrap-around 위험이 있으므로(pitfall 4-2), **반드시 청크 단위
//! AEAD(STREAM)** 로 프레이밍한다. `aead::stream`의 `EncryptorBE32`/`DecryptorBE32`가
//! 청크별 nonce(7바이트 prefix + 4바이트 카운터 + 1바이트 last 플래그)를 자동 관리하고
//! 마지막 청크에 종료 플래그를 박아 **truncation을 탐지**한다.
//!
//! ## 와이어 포맷(format_version=1 고정)
//! ```text
//! [magic "XBAESG01" 8B][nonce_prefix 7B]( [len u32 BE][ciphertext+tag] )*  // 마지막 프레임은 last
//! ```
//! - 청크 평문 크기는 [`CHUNK_SIZE`](64KiB) 고정 — 변경 시 format_version을 올린다.
//! - 각 프레임은 GCM 태그(16B)를 포함하므로 변조·자름을 탐지한다.
//! - 키는 env 참조(32바이트 hex)로 주입한다 — 키 자체는 manifest·argv에 담지 않는다.
//!
//! ## 키 소스
//! 키는 32바이트(256bit)를 hex(64자)로 인코딩한 문자열을 env에서 읽는다. config
//! `features.encryption.algorithm="aes-256-gcm"` 선택 시 활성화하며, 호출자(backup.rs)가
//! env 변수명을 해석해 [`AesGcmEncryptStage::from_hex_key`]로 주입한다.

use aead::stream::{DecryptorBE32, EncryptorBE32};
use aes_gcm::aead::generic_array::GenericArray;
use aes_gcm::{Aes256Gcm, KeyInit};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::error::{Result, XBackupError};
use crate::pipeline::stage::PipelineStage;
use crate::storage::BoxAsyncRead;

/// 암호화 알고리즘 식별자(manifest `encryption.algorithm`).
pub const ALGORITHM_AES_GCM: &str = "aes-256-gcm";

/// 와이어 포맷 매직 — 포맷·버전을 1바이트 수준에서 식별한다(변경 시 버전 증가).
const MAGIC: &[u8; 8] = b"XBAESG01";

/// STREAM nonce prefix 길이(BE32: 12바이트 GCM nonce − 5바이트 카운터/플래그 = 7).
const NONCE_PREFIX_LEN: usize = 7;

/// 청크 평문 크기(64KiB). 변경 시 format_version을 올려야 한다(pitfall 4-2).
const CHUNK_SIZE: usize = 64 * 1024;

/// GCM 인증 태그 길이(바이트).
const TAG_LEN: usize = 16;

/// duplex 백프레셔 버퍼 — 청크 1개 + 프레임 오버헤드를 수용.
const DUPLEX_BUF_BYTES: usize = CHUNK_SIZE + 64;

/// 32바이트(256bit) AES 키.
type Key = [u8; 32];

/// hex 문자열(64자)을 32바이트 AES 키로 디코드한다.
fn decode_hex_key(hex_key: &str) -> Result<Key> {
    let bytes = hex::decode(hex_key.trim())
        .map_err(|e| XBackupError::Config(format!("AES 키 hex 디코드 실패: {e}")))?;
    if bytes.len() != 32 {
        return Err(XBackupError::Config(format!(
            "AES-256 키는 32바이트(hex 64자)여야 합니다(got {}바이트)",
            bytes.len()
        )));
    }
    let mut key = [0u8; 32];
    key.copy_from_slice(&bytes);
    Ok(key)
}

/// AES-256-GCM STREAM 암호화 단계(정방향).
pub struct AesGcmEncryptStage {
    key: Key,
}

impl AesGcmEncryptStage {
    /// hex(64자) 키로 암호화 단계를 만든다.
    pub fn from_hex_key(hex_key: &str) -> Result<Self> {
        Ok(Self {
            key: decode_hex_key(hex_key)?,
        })
    }

    /// raw 32바이트 키로 단계를 만든다(테스트·프로그램 구성용).
    pub fn from_key(key: Key) -> Self {
        Self { key }
    }
}

impl PipelineStage for AesGcmEncryptStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        // 펌프 실패(encrypt_next/last 오류 등)는 PumpReader가 io::Error로 surface한다(C1).
        let key = self.key;
        crate::crypto::pump_reader(DUPLEX_BUF_BYTES, move |writer| {
            pump_encrypt(key, input, writer)
        })
    }

    fn name(&self) -> &'static str {
        ALGORITHM_AES_GCM
    }
}

/// 평문을 청크 단위로 읽어 STREAM 암호화 프레임으로 `sink`에 쓴다.
async fn pump_encrypt(
    key: Key,
    mut input: BoxAsyncRead,
    mut sink: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    // 랜덤 nonce prefix(7B) — STREAM이 청크별 nonce를 prefix||counter||last로 합성.
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    rand_fill(&mut nonce_prefix);

    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    let mut stream = EncryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_prefix));

    // 헤더: 매직 + nonce prefix.
    sink.write_all(MAGIC).await?;
    sink.write_all(&nonce_prefix).await?;

    // 평문을 CHUNK_SIZE씩 읽되, "다음 청크가 있는지" 한 청크 미리보기로 마지막을 판정한다.
    let mut current = read_exact_chunk(&mut input).await?;
    loop {
        let next = read_exact_chunk(&mut input).await?;
        if next.is_empty() {
            // current가 마지막 청크(빈 입력이면 current도 빈 last 프레임).
            let frame = stream
                .encrypt_last(current.as_slice())
                .map_err(|e| std::io::Error::other(format!("AES-GCM encrypt_last 실패: {e}")))?;
            write_frame(&mut sink, &frame).await?;
            break;
        }
        let frame = stream
            .encrypt_next(current.as_slice())
            .map_err(|e| std::io::Error::other(format!("AES-GCM encrypt_next 실패: {e}")))?;
        write_frame(&mut sink, &frame).await?;
        current = next;
    }

    sink.shutdown().await?;
    Ok(())
}

/// AES-256-GCM STREAM 복호화 단계(역방향).
pub struct AesGcmDecryptStage {
    key: Key,
}

impl AesGcmDecryptStage {
    /// hex(64자) 키로 복호화 단계를 만든다.
    pub fn from_hex_key(hex_key: &str) -> Result<Self> {
        Ok(Self {
            key: decode_hex_key(hex_key)?,
        })
    }

    /// raw 32바이트 키로 단계를 만든다(테스트·프로그램 구성용).
    pub fn from_key(key: Key) -> Self {
        Self { key }
    }
}

impl PipelineStage for AesGcmDecryptStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        // 변조/자름/키 오류로 인한 펌프 실패는 PumpReader가 io::Error로 surface한다(C1) —
        // 복구에서 잘린 평문이 다음 단계로 흘러가는 것을 막는다.
        let key = self.key;
        crate::crypto::pump_reader(DUPLEX_BUF_BYTES, move |writer| {
            pump_decrypt(key, input, writer)
        })
    }

    fn name(&self) -> &'static str {
        ALGORITHM_AES_GCM
    }
}

/// STREAM 프레임을 읽어 복호화한 평문을 `sink`에 쓴다. 변조·자름은 GCM 태그·last
/// 플래그로 탐지되어 에러로 전파된다.
async fn pump_decrypt(
    key: Key,
    mut input: BoxAsyncRead,
    mut sink: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    // 헤더 검증.
    let mut magic = [0u8; 8];
    input.read_exact(&mut magic).await?;
    if &magic != MAGIC {
        return Err(std::io::Error::other("AES-GCM 매직 불일치(포맷·키 오류)"));
    }
    let mut nonce_prefix = [0u8; NONCE_PREFIX_LEN];
    input.read_exact(&mut nonce_prefix).await?;

    let cipher = Aes256Gcm::new(GenericArray::from_slice(&key));
    let mut stream = DecryptorBE32::from_aead(cipher, GenericArray::from_slice(&nonce_prefix));

    // 프레임을 미리보기로 한 개씩 앞서 읽어 마지막 프레임을 last로 복호화한다.
    let mut current = read_frame(&mut input).await?;
    loop {
        let next = read_frame(&mut input).await?;
        match (&current, &next) {
            (Some(frame), None) => {
                // 마지막 프레임.
                let plain = stream.decrypt_last(frame.as_slice()).map_err(|_| {
                    std::io::Error::other("AES-GCM decrypt_last 실패(변조/키 오류)")
                })?;
                sink.write_all(&plain).await?;
                break;
            }
            (Some(frame), Some(_)) => {
                let plain = stream.decrypt_next(frame.as_slice()).map_err(|_| {
                    std::io::Error::other("AES-GCM decrypt_next 실패(변조/키 오류)")
                })?;
                sink.write_all(&plain).await?;
                current = next;
            }
            (None, _) => {
                // 프레임이 하나도 없음 — 빈 STREAM은 최소 1개의 last 프레임을 가진다.
                return Err(std::io::Error::other("AES-GCM 프레임 없음(잘린 입력)"));
            }
        }
    }

    sink.shutdown().await?;
    Ok(())
}

/// `[len u32 BE][bytes]` 프레임을 쓴다.
async fn write_frame(sink: &mut tokio::io::DuplexStream, frame: &[u8]) -> std::io::Result<()> {
    let len =
        u32::try_from(frame.len()).map_err(|_| std::io::Error::other("프레임 길이 초과(u32)"))?;
    sink.write_all(&len.to_be_bytes()).await?;
    sink.write_all(frame).await?;
    Ok(())
}

/// `[len u32 BE][bytes]` 프레임을 읽는다. EOF면 `None`. 길이가 비정상이면 에러.
async fn read_frame(input: &mut BoxAsyncRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    match input.read_exact(&mut len_buf).await {
        Ok(_) => {}
        // 정확히 프레임 경계에서의 EOF — 더 읽을 프레임 없음.
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    // 빈 평문 last 프레임도 태그(16B)는 있으므로 최소 길이는 TAG_LEN, 최대는 청크+태그.
    if !(TAG_LEN..=CHUNK_SIZE + TAG_LEN).contains(&len) {
        return Err(std::io::Error::other(format!(
            "AES-GCM 프레임 길이 비정상: {len}"
        )));
    }
    let mut frame = vec![0u8; len];
    input.read_exact(&mut frame).await?;
    Ok(Some(frame))
}

/// 입력에서 최대 [`CHUNK_SIZE`]바이트를 채워 읽는다(EOF면 짧거나 빈 Vec).
async fn read_exact_chunk(input: &mut BoxAsyncRead) -> std::io::Result<Vec<u8>> {
    let mut buf = vec![0u8; CHUNK_SIZE];
    let mut filled = 0;
    while filled < CHUNK_SIZE {
        let n = input.read(&mut buf[filled..]).await?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    Ok(buf)
}

/// 암호학적 난수로 버퍼를 채운다(nonce prefix용).
fn rand_fill(buf: &mut [u8]) {
    use aead::rand_core::RngCore;
    aead::OsRng.fill_bytes(buf);
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;

    fn reader_from(data: &[u8]) -> BoxAsyncRead {
        Box::pin(std::io::Cursor::new(data.to_vec()))
    }

    async fn drain_result(reader: BoxAsyncRead) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut r = reader;
        r.read_to_end(&mut out).await?;
        Ok(out)
    }

    const KEY: Key = [7u8; 32];

    /// 단일 청크 미만 페이로드의 round-trip.
    #[tokio::test]
    async fn aes_round_trip_small() {
        let payload = b"small secret payload".to_vec();
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let ct = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();
        assert_ne!(ct, payload);
        assert!(ct.starts_with(MAGIC), "매직 헤더 부재");

        let decrypt = Box::new(AesGcmDecryptStage::from_key(KEY));
        let restored = drain_result(decrypt.wrap(reader_from(&ct))).await.unwrap();
        assert_eq!(restored, payload);
    }

    /// 다중 청크(>64KiB)에 걸친 STREAM round-trip — 청크 경계·last 프레임 검증.
    #[tokio::test]
    async fn aes_round_trip_multi_chunk() {
        // 2.5청크 분량.
        let payload: Vec<u8> = (0..(CHUNK_SIZE * 5 / 2)).map(|i| (i % 251) as u8).collect();
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let ct = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();

        let decrypt = Box::new(AesGcmDecryptStage::from_key(KEY));
        let restored = drain_result(decrypt.wrap(reader_from(&ct))).await.unwrap();
        assert_eq!(restored, payload, "다중 청크 round-trip 실패");
    }

    /// 빈 입력도 round-trip이 성립한다(빈 last 프레임).
    #[tokio::test]
    async fn aes_round_trip_empty() {
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let ct = drain_result(encrypt.wrap(reader_from(b""))).await.unwrap();
        let decrypt = Box::new(AesGcmDecryptStage::from_key(KEY));
        let restored = drain_result(decrypt.wrap(reader_from(&ct))).await.unwrap();
        assert!(restored.is_empty());
    }

    /// 암호문 변조 시 복호화가 실패해야 한다(GCM 태그 무결성).
    #[tokio::test]
    async fn tampered_ciphertext_fails() {
        let payload = b"integrity matters".to_vec();
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let mut ct = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();

        // 헤더 이후(매직 8 + nonce 7 + len 4 = 19)의 첫 ciphertext 바이트를 뒤집는다.
        let idx = MAGIC.len() + NONCE_PREFIX_LEN + 4;
        ct[idx] ^= 0xFF;

        let decrypt = Box::new(AesGcmDecryptStage::from_key(KEY));
        let result = drain_result(decrypt.wrap(reader_from(&ct))).await;
        // C1: 변조는 GCM 태그 실패 → 펌프 Err → EOF 대신 에러로 surface돼야 한다.
        assert!(result.is_err(), "변조 복호화 실패가 surface되지 않음(C1)");
        let _ = payload;
    }

    /// 틀린 키로는 복호화에 실패한다.
    #[tokio::test]
    async fn wrong_key_fails() {
        let payload = b"key-bound secret".to_vec();
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let ct = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();

        let wrong = [9u8; 32];
        let decrypt = Box::new(AesGcmDecryptStage::from_key(wrong));
        let result = drain_result(decrypt.wrap(reader_from(&ct))).await;
        // C1: 틀린 키는 GCM 인증 실패 → 에러로 surface.
        assert!(result.is_err(), "틀린 키 복호화 실패가 surface되지 않음(C1)");
        let _ = payload;
    }

    /// 마지막 청크가 잘리면(truncation) 복호화가 실패해야 한다(last 플래그).
    #[tokio::test]
    async fn truncation_fails() {
        // 2청크 만들고 마지막 프레임을 통째로 제거 → DecryptorBE32가 last를 못 받아 실패.
        let payload: Vec<u8> = (0..(CHUNK_SIZE + 100)).map(|i| (i % 200) as u8).collect();
        let encrypt = Box::new(AesGcmEncryptStage::from_key(KEY));
        let ct = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();

        // 헤더 다음 첫 프레임(len + ciphertext)만 남기고 잘라낸다.
        let header = MAGIC.len() + NONCE_PREFIX_LEN;
        let first_len = {
            let lb = [ct[header], ct[header + 1], ct[header + 2], ct[header + 3]];
            u32::from_be_bytes(lb) as usize
        };
        let cut = header + 4 + first_len; // 첫 프레임 끝(마지막 프레임 제거).
        let truncated = ct[..cut].to_vec();

        let decrypt = Box::new(AesGcmDecryptStage::from_key(KEY));
        let result = drain_result(decrypt.wrap(reader_from(&truncated))).await;
        // 첫 프레임은 encrypt_next였으므로 decrypt_last로 처리되며 인증 실패.
        // C1: 자름(truncation)은 last 플래그 불일치 → 에러로 surface돼야 한다.
        assert!(result.is_err(), "자름 복호화 실패가 surface되지 않음(C1)");
        let _ = payload;
    }

    /// 키 hex 디코드: 길이·형식 검증.
    #[test]
    fn hex_key_validation() {
        let valid = "00".repeat(32);
        assert!(AesGcmEncryptStage::from_hex_key(&valid).is_ok());
        // 너무 짧음.
        assert!(AesGcmEncryptStage::from_hex_key("00").is_err());
        // 비 hex.
        assert!(AesGcmEncryptStage::from_hex_key(&"zz".repeat(32)).is_err());
    }
}
