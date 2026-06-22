//! age(X25519) 암호화 단계 — 기본 암호화 경로(PRD §8.1).
//!
//! 공개키(recipient)로만 암호화하므로 백업 호스트는 공개키만 보유하면 된다 — 개인키
//! 없이는 복호화 불가(공개키-only 격리, §8.1/§8.5). age는 내부적으로
//! ChaCha20-Poly1305 STREAM(64KiB 청크)으로 대용량을 안전하게 처리한다.
//!
//! ## AsyncRead↔AsyncWrite 브리지
//! age async API는 **쓰기 기반**이다: [`Encryptor::wrap_async_output`]는
//! `futures::io::AsyncWrite`를 받아 암호문을 그쪽으로 쓴다. 하지만 [`PipelineStage`]는
//! `AsyncRead → AsyncRead` 어댑터다. 그래서 [`tokio::io::duplex`] 파이프를 만들고,
//! **백그라운드 태스크**가 입력 reader를 age 암호화 writer로 펌프한다. duplex의 읽기
//! 끝을 다음 단계 reader로 반환한다. duplex 버퍼가 자연 백프레셔를 제공하므로 전 구간
//! 스트리밍이 유지된다(메모리 상수, R1).
//!
//! futures-io ↔ tokio-io 트레이트 차이는 `tokio_util::compat`로 변환한다.
//!
//! ## 복호화
//! [`AgeDecryptStage`]는 개인키(identity)로 복호화한다. [`Decryptor::new_async`]가
//! 반환하는 futures-io reader를 tokio reader로 변환해 다음 단계로 흘린다. 개인키가
//! 없거나 틀리면 복호화에 실패한다(테스트로 보장).

use std::str::FromStr;

use age::secrecy::ExposeSecret;
use age::x25519;
use age::{Decryptor, Identity, Recipient};
use futures::io::{AsyncReadExt as FuturesAsyncReadExt, AsyncWriteExt as FuturesAsyncWriteExt};
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

use crate::error::{Result, XBackupError};
use crate::pipeline::stage::PipelineStage;
use crate::storage::BoxAsyncRead;

/// 암호화 알고리즘 식별자(manifest `encryption.algorithm`).
pub const ALGORITHM_AGE: &str = "age";

/// duplex 파이프 버퍼 크기 — age STREAM 청크(64KiB)와 균형을 맞춘 백프레셔 버퍼.
const DUPLEX_BUF_BYTES: usize = 64 * 1024;

/// age 암호화 단계 — recipient 공개키로 암호화한다(정방향).
pub struct AgeEncryptStage {
    recipient: x25519::Recipient,
}

impl AgeEncryptStage {
    /// recipient 파일(공개키 `age1...`)을 읽어 암호화 단계를 만든다.
    ///
    /// 파일에는 한 줄짜리 age recipient 공개키가 있어야 한다(주석·빈 줄 허용).
    pub fn from_recipient_file(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("age recipient 파일 읽기 실패({path}): {e}"))
        })?;
        let recipient = parse_recipient(&raw).ok_or_else(|| {
            XBackupError::Config(format!(
                "age recipient 파일에 유효한 공개키(age1...)가 없습니다: {path}"
            ))
        })?;
        Ok(Self { recipient })
    }

    /// recipient 공개키로 직접 단계를 만든다(테스트·프로그램 구성용).
    pub fn from_recipient(recipient: x25519::Recipient) -> Self {
        Self { recipient }
    }

    /// recipient 공개키 식별자(`age1...` 지문) — manifest `encryption.key_id` 기록용.
    pub fn key_id(&self) -> String {
        self.recipient.to_string()
    }
}

impl PipelineStage for AgeEncryptStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        // 펌프 실패는 PumpReader가 EOF 대신 io::Error로 surface한다(C1) — duplex writer가
        // 그냥 drop돼 다음 단계가 잘린 산출물을 정상 종료로 오인하는 것을 막는다.
        let recipient = self.recipient;
        crate::crypto::pump_reader(DUPLEX_BUF_BYTES, move |writer| {
            pump_encrypt(recipient, input, writer)
        })
    }

    fn name(&self) -> &'static str {
        ALGORITHM_AGE
    }
}

/// age STREAM 펌프 청크 크기(입력 읽기 단위).
const PUMP_CHUNK: usize = 64 * 1024;

/// 입력 평문을 age로 암호화해 `sink`(tokio AsyncWrite)로 펌프한다.
async fn pump_encrypt(
    recipient: x25519::Recipient,
    mut input: BoxAsyncRead,
    mut sink: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    // Encryptor는 &dyn Recipient 이터레이터를 받는다.
    let recipients: Vec<Box<dyn Recipient + Send>> = vec![Box::new(recipient)];
    let encryptor =
        age::Encryptor::with_recipients(recipients.iter().map(|r| r.as_ref() as &dyn Recipient))
            .map_err(std::io::Error::other)?;

    // sink(tokio AsyncWrite)를 futures AsyncWrite로 변환해 age에 넘긴다(age는 futures-io).
    let futures_sink = (&mut sink).compat_write();
    let mut age_writer = encryptor
        .wrap_async_output(futures_sink)
        .await
        .map_err(std::io::Error::other)?;

    // 평문을 청크 단위로 읽어 age writer(futures AsyncWrite)로 흘린다.
    let mut buf = vec![0u8; PUMP_CHUNK];
    loop {
        let n = input.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        age_writer
            .write_all(&buf[..n])
            .await
            .map_err(std::io::Error::other)?;
    }

    // age STREAM 마지막 청크 flush·finalize(close 누락 시 truncation).
    age_writer.close().await.map_err(std::io::Error::other)?;
    sink.shutdown().await?;
    Ok(())
}

/// age 복호화 단계 — 개인키(identity)로 복호화한다(역방향, 복구·verify --deep).
pub struct AgeDecryptStage {
    identity: x25519::Identity,
}

impl AgeDecryptStage {
    /// identity 파일(개인키 `AGE-SECRET-KEY-1...`)을 읽어 복호화 단계를 만든다.
    pub fn from_identity_file(path: &str) -> Result<Self> {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            XBackupError::Config(format!("age identity 파일 읽기 실패({path}): {e}"))
        })?;
        let identity = parse_identity(&raw).ok_or_else(|| {
            XBackupError::Config(format!(
                "age identity 파일에 유효한 개인키(AGE-SECRET-KEY-1...)가 없습니다: {path}"
            ))
        })?;
        Ok(Self { identity })
    }

    /// identity 개인키로 직접 단계를 만든다(테스트·프로그램 구성용).
    pub fn from_identity(identity: x25519::Identity) -> Self {
        Self { identity }
    }
}

impl PipelineStage for AgeDecryptStage {
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
        // 개인키 없음/불일치/변조/자름은 펌프가 Err로 반환하고, PumpReader가 그 에러를
        // EOF 대신 surface한다(C1) — 복구에서 잘린 평문이 mongorestore로 흘러가는 것을 막는다.
        let identity = self.identity;
        crate::crypto::pump_reader(DUPLEX_BUF_BYTES, move |writer| {
            pump_decrypt(identity, input, writer)
        })
    }

    fn name(&self) -> &'static str {
        ALGORITHM_AGE
    }
}

/// age 암호문을 identity로 복호화해 `sink`로 펌프한다.
async fn pump_decrypt(
    identity: x25519::Identity,
    input: BoxAsyncRead,
    mut sink: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    // 입력(tokio AsyncRead)을 futures AsyncRead로 변환해 Decryptor에 넘긴다.
    let buffered = BufReader::new(input);
    let futures_input = buffered.compat();

    let decryptor = Decryptor::new_async(futures_input)
        .await
        .map_err(std::io::Error::other)?;

    // identity 참조 이터레이터는 decrypt_async 호출 동안만 살아 있으면 된다 — 반환된
    // reader는 자체 상태를 소유한다. ids는 await을 가로지르지 않도록 즉시 소비한다.
    let mut plaintext_reader = {
        let id_ref: &dyn Identity = &identity;
        decryptor
            .decrypt_async(std::iter::once(id_ref))
            .map_err(std::io::Error::other)?
    };

    // 복호화 reader(futures AsyncRead)를 청크 단위로 읽어 sink로 흘린다.
    let mut buf = vec![0u8; PUMP_CHUNK];
    loop {
        let n = plaintext_reader
            .read(&mut buf)
            .await
            .map_err(std::io::Error::other)?;
        if n == 0 {
            break;
        }
        sink.write_all(&buf[..n]).await?;
    }
    sink.shutdown().await?;
    Ok(())
}

/// recipient 파일 본문에서 첫 유효 공개키를 파싱한다(주석 `#`·빈 줄 무시).
fn parse_recipient(raw: &str) -> Option<x25519::Recipient> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| x25519::Recipient::from_str(l).ok())
}

/// identity 파일 본문에서 첫 유효 개인키를 파싱한다(주석 `#`·빈 줄 무시).
fn parse_identity(raw: &str) -> Option<x25519::Identity> {
    raw.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .find_map(|l| x25519::Identity::from_str(l).ok())
}

/// age(X25519) 키쌍을 새로 만들어 파일로 쓴다 — 개인키(identity)를 `key_path`에
/// **0600**으로, 공개키(recipient)를 `pub_path`에 일반 권한으로 기록한다.
///
/// `init` 마법사의 "공개키 파일이 없으면 만들어 주기"와 `gen_age_key` 예제가 공유하는
/// 단일 진입점이다(중복 구현 방지). 부모 디렉터리는 필요 시 생성한다. 반환값은
/// recipient 공개키 문자열(`age1...`) — 생성 안내 출력용.
///
/// 시크릿(개인키)은 0600으로 격리되며, 백업 호스트엔 공개키만 두면 된다(§8.1/§8.5).
pub fn generate_keypair_files(key_path: &std::path::Path, pub_path: &std::path::Path) -> Result<String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;

    let identity = x25519::Identity::generate();
    let recipient = identity.to_public();

    for p in [key_path, pub_path] {
        if let Some(parent) = p.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    XBackupError::Config(format!("키 디렉터리 생성 실패({}): {e}", parent.display()))
                })?;
            }
        }
    }

    // 개인키 — 0600(소유자 전용)으로 격리 기록.
    let mut key_file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(key_path)
        .map_err(|e| {
            XBackupError::Config(format!("개인키 파일 생성 실패({}): {e}", key_path.display()))
        })?;
    writeln!(key_file, "{}", identity.to_string().expose_secret())
        .map_err(|e| XBackupError::Config(format!("개인키 기록 실패({}): {e}", key_path.display())))?;

    // 공개키(recipient) — 시크릿 아님.
    std::fs::write(pub_path, format!("{recipient}\n")).map_err(|e| {
        XBackupError::Config(format!("공개키 파일 기록 실패({}): {e}", pub_path.display()))
    })?;

    Ok(recipient.to_string())
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

    /// generate_keypair_files: 키쌍을 만들고, 그 공개키로 암호화한 산출물을 같은
    /// 개인키로 복호화하면 원본이 복원된다(생성물이 실제로 짝이 맞는 유효한 키쌍).
    #[tokio::test]
    async fn generated_keypair_round_trips() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("age.key");
        let pub_path = dir.path().join("nested").join("age.pub"); // 부모 디렉터리 자동 생성 확인

        let recipient = generate_keypair_files(&key_path, &pub_path).unwrap();
        assert!(recipient.starts_with("age1"), "recipient는 age1 공개키여야 함");
        assert!(key_path.exists() && pub_path.exists());

        // 개인키는 0600으로 격리돼야 한다.
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "개인키는 0600이어야 함");

        // 공개키 파일로 암호화 → 개인키 파일로 복호화 → 원본 복원.
        let plaintext = b"generated-keypair-round-trip".to_vec();
        let encrypt = Box::new(
            AgeEncryptStage::from_recipient_file(pub_path.to_str().unwrap()).unwrap(),
        );
        let encrypted = drain_result(encrypt.wrap(reader_from(&plaintext)))
            .await
            .unwrap();
        let decrypt = Box::new(
            AgeDecryptStage::from_identity_file(key_path.to_str().unwrap()).unwrap(),
        );
        let decrypted = drain_result(decrypt.wrap(reader_from(&encrypted)))
            .await
            .unwrap();
        assert_eq!(decrypted, plaintext);
    }

    /// age 암호화 → 같은 키로 복호화가 원본을 복원한다(round-trip).
    #[tokio::test]
    async fn age_round_trip() {
        let identity = x25519::Identity::generate();
        let recipient = identity.to_public();
        let payload = b"top-secret BSON archive bytes \x00\x01\x02".to_vec();

        let encrypt = Box::new(AgeEncryptStage::from_recipient(recipient));
        let ciphertext = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();
        // 암호문은 평문과 달라야 한다(실제 암호화).
        assert_ne!(ciphertext, payload);
        // age 컨테이너 매직(`age-encryption.org`)이 포함되어야 한다.
        assert!(
            ciphertext
                .windows(b"age-encryption.org".len())
                .any(|w| w == b"age-encryption.org"),
            "age 헤더 매직 부재"
        );

        let decrypt = Box::new(AgeDecryptStage::from_identity(identity));
        let restored = drain_result(decrypt.wrap(reader_from(&ciphertext)))
            .await
            .unwrap();
        assert_eq!(restored, payload, "age round-trip 후 원본 불일치");
    }

    /// 개인키(identity) 없이/다른 키로는 복호화에 실패해야 한다(공개키 격리, §8.1).
    #[tokio::test]
    async fn decrypt_with_wrong_key_fails() {
        let recipient_identity = x25519::Identity::generate();
        let recipient = recipient_identity.to_public();
        let payload = b"secret".to_vec();

        let encrypt = Box::new(AgeEncryptStage::from_recipient(recipient));
        let ciphertext = drain_result(encrypt.wrap(reader_from(&payload)))
            .await
            .unwrap();

        // 전혀 다른 키쌍으로 복호화 시도 → 실패(plaintext 미복원).
        let wrong_identity = x25519::Identity::generate();
        let decrypt = Box::new(AgeDecryptStage::from_identity(wrong_identity));
        let result = drain_result(decrypt.wrap(reader_from(&ciphertext))).await;
        // C1: 복호화 펌프 실패는 EOF가 아니라 에러로 surface돼야 한다 — 잘린/빈 평문이
        // 정상 종료로 mongorestore에 전달되는 것을 막는다.
        assert!(
            result.is_err(),
            "틀린 키 복호화 실패가 surface되지 않음(C1) — 평문 격리/무결성 위반"
        );
        let _ = payload;
    }

    /// recipient 파일 파싱이 주석·빈 줄을 건너뛰고 공개키를 찾는다.
    #[test]
    fn parse_recipient_skips_comments() {
        let id = x25519::Identity::generate();
        let pub_str = id.to_public().to_string();
        let body = format!("# age recipient\n\n{pub_str}\n");
        let parsed = parse_recipient(&body).expect("공개키 파싱 실패");
        assert_eq!(parsed.to_string(), pub_str);
    }

    /// 잘못된 본문은 None을 반환한다(상위에서 Config 에러로 변환).
    #[test]
    fn parse_recipient_rejects_garbage() {
        assert!(parse_recipient("not-a-key\n# comment\n").is_none());
    }
}
