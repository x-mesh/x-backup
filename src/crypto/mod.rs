//! Crypto 계층 — 스트리밍 암호화(PRD §8).
//!
//! 암호화는 [`PipelineStage`](crate::pipeline::stage::PipelineStage)로 구현되어
//! 백업 파이프라인의 **마지막 변환 단계**가 된다(PRD §8.4: compress → encrypt 고정).
//! 두 알고리즘을 지원한다:
//! - [`age`]: 기본. X25519 공개키로만 암호화 → 백업 호스트는 공개키만 보유(§8.1 격리).
//! - [`aes_gcm`]: 대안. AES-256-GCM + STREAM 청크 AEAD(§8.2, 폐쇄 환경 대칭 키).
//!
//! ## 메타데이터(키 비저장, §8.3)
//! manifest에는 알고리즘과 **키 식별자**(age recipient 지문 또는 키 소스 라벨)만
//! 기록한다 — 키 자체는 절대 담지 않는다([`EncryptionMeta`]).
//!
//! ## 단계 선택
//! 호출자(backup.rs)는 config·CLI를 해석해 [`build_encrypt_stage`]로 암호화 단계와
//! manifest 메타를 함께 얻는다. 복구(t5)는 [`build_decrypt_stage`]로 manifest 메타
//! 기반 역방향 단계를 얻는다.

pub mod aes_gcm;
pub mod age;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, DuplexStream, ReadBuf};
use tokio::task::JoinHandle;

use crate::config::file::EncryptionConfig;
use crate::error::{Result, XBackupError};
use crate::manifest::schema::EncryptionMeta;
use crate::pipeline::stage::PipelineStage;
use crate::storage::BoxAsyncRead;

pub use self::aes_gcm::{AesGcmDecryptStage, AesGcmEncryptStage, ALGORITHM_AES_GCM};
pub use self::age::{generate_keypair_files, AgeDecryptStage, AgeEncryptStage, ALGORITHM_AGE};

/// 암복호 펌프 reader 어댑터 — 펌프 태스크의 [`JoinHandle`]을 붙잡아, duplex가
/// EOF를 줄 때 **펌프 결과를 확인한다**.
///
/// ## 배경(C1 — 펌프 에러 삼킴 방지)
/// 암호화/복호화는 `tokio::io::duplex` 파이프 + 백그라운드 펌프 태스크로 스트리밍한다.
/// 펌프가 중간에 실패하면 writer가 drop되는데, duplex의 읽기 끝은 이를 **에러가 아니라
/// 깨끗한 EOF(Ok(0))** 로 본다. 과거 구현은 JoinHandle을 버려서, 잘린 산출물이
/// 정상 종료처럼 흘러갔다 — 백업에서는 `status: Complete`로 기록되고(exit 0),
/// 복구에서는 **잘린 평문이 mongorestore/pg_restore에 전달**됐다.
///
/// 이 어댑터는 duplex가 EOF를 줄 때 펌프 태스크의 결과를 폴링해:
/// - 펌프가 `Ok` → 정상 EOF,
/// - 펌프가 `Err(e)` → EOF 대신 그 `io::Error`를 surface(다운스트림이 실패로 인지),
/// - 태스크 패닉(JoinError) → `io::Error`로 변환.
///
/// 펌프가 일부 바이트를 쓴 뒤 실패한 경우, 그 바이트는 먼저 정상 전달되고 **마지막에
/// 에러가 surface**되므로 다운스트림(체크섬·put_stream·복구 소비자)이 부분 산출물을
/// 끝까지 정상으로 오인하지 않는다.
struct PumpReader {
    reader: DuplexStream,
    handle: Option<JoinHandle<std::io::Result<()>>>,
}

impl AsyncRead for PumpReader {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        // PumpReader의 필드는 모두 Unpin이므로 안전하게 가변 참조를 얻는다.
        let me = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut me.reader).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                if buf.filled().len() != before {
                    // 데이터를 읽음 — 그대로 전달.
                    return Poll::Ready(Ok(()));
                }
                // duplex가 EOF — 펌프 태스크 결과로 정상 종료인지 실패인지 판별한다.
                let Some(handle) = me.handle.as_mut() else {
                    // 이미 검증한 뒤의 반복 EOF — 진짜 종료.
                    return Poll::Ready(Ok(()));
                };
                match Pin::new(handle).poll(cx) {
                    Poll::Ready(Ok(Ok(()))) => {
                        me.handle = None;
                        Poll::Ready(Ok(())) // 펌프 정상 완료 → 정상 EOF
                    }
                    Poll::Ready(Ok(Err(e))) => {
                        me.handle = None;
                        Poll::Ready(Err(e)) // 펌프 실패 → EOF 대신 에러 surface
                    }
                    Poll::Ready(Err(join_err)) => {
                        me.handle = None;
                        Poll::Ready(Err(std::io::Error::other(format!(
                            "암복호 펌프 태스크 비정상 종료: {join_err}"
                        ))))
                    }
                    // 펌프가 아직 종료 직전(드물게) — 태스크 완료 시 waker로 재폴링된다.
                    Poll::Pending => Poll::Pending,
                }
            }
            other => other,
        }
    }
}

/// 펌프 패턴 공통 헬퍼 — duplex를 만들고 펌프 future를 spawn한 뒤, 펌프 실패를
/// 에러로 surface하는 [`PumpReader`]를 반환한다(age/aes_gcm 공용, C1).
///
/// `make_pump`는 duplex의 writer를 받아 펌프 future를 만든다(입력 reader·키 등은
/// 호출 측 클로저가 캡처). 펌프 future는 `Send + 'static`이어야 한다(태스크로 spawn).
pub(crate) fn pump_reader<Fut>(
    buf_bytes: usize,
    make_pump: impl FnOnce(DuplexStream) -> Fut,
) -> BoxAsyncRead
where
    Fut: Future<Output = std::io::Result<()>> + Send + 'static,
{
    let (writer, reader) = tokio::io::duplex(buf_bytes);
    let handle = tokio::spawn(make_pump(writer));
    Box::pin(PumpReader {
        reader,
        handle: Some(handle),
    })
}

/// 복호화 키 소스 — 복구(t5)·verify --deep에서 개인키/대칭키를 어디서 읽을지.
///
/// 백업 경로(공개키-only 호스트)에는 개인키가 없으므로 암호화 단계 생성에는 쓰지 않고,
/// 복호화 단계([`build_decrypt_stage`]) 생성에만 쓰인다(§8.5 키 격리).
pub enum DecryptKeySource {
    /// age identity 파일 경로(개인키 `AGE-SECRET-KEY-1...`).
    AgeIdentityFile(String),
    /// AES-256-GCM 대칭 키(hex 64자).
    AesHexKey(String),
}

/// config·CLI를 해석해 암호화 단계와 manifest 메타를 만든다(백업 정방향).
///
/// - `--no-encrypt`(CLI)면 호출자가 이 함수를 호출하지 않는다(평문). 여기서는 항상
///   암호화 단계를 만든다.
/// - 알고리즘은 `enc.algorithm`(기본 `"age"`)을 따른다.
///   - `"age"`: `enc.recipient_file` 공개키로 암호화. key_id = recipient 지문.
///   - `"aes-256-gcm"`: `aes_key_hex`(env로 해석된 32바이트 hex)로 암호화.
///     key_id는 키 *값*이 아니라 키 소스 라벨만 기록한다(§8.3).
///
/// `aes_key_hex`는 호출자가 env에서 읽어 넘긴다(이 계층은 env 접근하지 않음).
pub fn build_encrypt_stage(
    enc: &EncryptionConfig,
    aes_key_hex: Option<&str>,
) -> Result<(Box<dyn PipelineStage>, EncryptionMeta)> {
    match enc.algorithm.as_str() {
        ALGORITHM_AGE => {
            let recipient_file = enc.recipient_file.as_deref().ok_or_else(|| {
                XBackupError::Config(
                    "age 암호화에는 features.encryption.recipient_file(공개키 경로)이 필요합니다"
                        .into(),
                )
            })?;
            let stage = AgeEncryptStage::from_recipient_file(recipient_file)?;
            let meta = EncryptionMeta {
                algorithm: ALGORITHM_AGE.to_string(),
                // recipient 공개키 지문(age1...)은 키 자체가 아니라 식별자다(§8.3).
                key_id: Some(stage.key_id()),
            };
            Ok((Box::new(stage), meta))
        }
        ALGORITHM_AES_GCM => {
            let key_hex = aes_key_hex.ok_or_else(|| {
                XBackupError::Config(
                    "aes-256-gcm 암호화에는 키(env 32바이트 hex)가 필요합니다".into(),
                )
            })?;
            let stage = AesGcmEncryptStage::from_hex_key(key_hex)?;
            let meta = EncryptionMeta {
                algorithm: ALGORITHM_AES_GCM.to_string(),
                // 대칭 키는 식별자를 따로 두지 않는다(키 값 비저장). 소스 라벨만 남긴다.
                key_id: Some("env".to_string()),
            };
            Ok((Box::new(stage), meta))
        }
        other => Err(XBackupError::Config(format!(
            "알 수 없는 암호화 알고리즘: '{other}'(age | aes-256-gcm)"
        ))),
    }
}

/// manifest 메타·키 소스로 복호화 단계를 만든다(복구 역방향, t5/`reverse_stack_for`).
///
/// `meta.algorithm`과 `key`가 일치해야 한다(age 메타에 AES 키를 주면 에러).
pub fn build_decrypt_stage(
    meta: &EncryptionMeta,
    key: &DecryptKeySource,
) -> Result<Box<dyn PipelineStage>> {
    match (meta.algorithm.as_str(), key) {
        (ALGORITHM_AGE, DecryptKeySource::AgeIdentityFile(path)) => {
            Ok(Box::new(AgeDecryptStage::from_identity_file(path)?))
        }
        (ALGORITHM_AES_GCM, DecryptKeySource::AesHexKey(hex)) => {
            Ok(Box::new(AesGcmDecryptStage::from_hex_key(hex)?))
        }
        (algo, _) => Err(XBackupError::Config(format!(
            "암호화 알고리즘 '{algo}'에 맞지 않는 복호화 키 소스"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn enc_config(algorithm: &str, recipient_file: Option<&str>) -> EncryptionConfig {
        EncryptionConfig {
            enabled: true,
            algorithm: algorithm.to_string(),
            recipient_file: recipient_file.map(str::to_string),
        }
    }

    /// build_encrypt_stage의 에러 exit code를 꺼낸다(성공 타입은 Debug가 아니므로 match).
    fn encrypt_err_code(cfg: &EncryptionConfig, key: Option<&str>) -> u8 {
        match build_encrypt_stage(cfg, key) {
            Ok(_) => panic!("성공하면 안 되는 케이스"),
            Err(e) => e.exit_code(),
        }
    }

    /// age 알고리즘인데 recipient_file이 없으면 Config 에러(exit 2).
    #[test]
    fn age_without_recipient_file_errs() {
        let cfg = enc_config("age", None);
        assert_eq!(encrypt_err_code(&cfg, None), 2);
    }

    /// aes-256-gcm인데 키가 없으면 Config 에러.
    #[test]
    fn aes_without_key_errs() {
        let cfg = enc_config("aes-256-gcm", None);
        assert_eq!(encrypt_err_code(&cfg, None), 2);
    }

    /// 알 수 없는 알고리즘은 Config 에러.
    #[test]
    fn unknown_algorithm_errs() {
        let cfg = enc_config("rot13", None);
        assert_eq!(encrypt_err_code(&cfg, None), 2);
    }

    /// aes-256-gcm 키가 있으면 단계·메타가 만들어지고 키 값은 메타에 없다(§8.3).
    #[test]
    fn aes_with_key_builds_meta_without_key_value() {
        let cfg = enc_config("aes-256-gcm", None);
        let key_hex = "ab".repeat(32);
        let (stage, meta) = build_encrypt_stage(&cfg, Some(&key_hex)).unwrap();
        assert_eq!(stage.name(), "aes-256-gcm");
        assert_eq!(meta.algorithm, "aes-256-gcm");
        // key_id는 소스 라벨일 뿐 키 값을 포함하지 않는다.
        assert_eq!(meta.key_id.as_deref(), Some("env"));
        assert!(!meta.key_id.as_deref().unwrap().contains(&key_hex));
    }

    /// 복호화 단계: 알고리즘과 키 소스 불일치는 에러.
    #[test]
    fn decrypt_key_source_mismatch_errs() {
        let age_meta = EncryptionMeta {
            algorithm: "age".to_string(),
            key_id: Some("age1xxx".to_string()),
        };
        let code =
            match build_decrypt_stage(&age_meta, &DecryptKeySource::AesHexKey("00".repeat(32))) {
                Ok(_) => panic!("불일치인데 성공함"),
                Err(e) => e.exit_code(),
            };
        assert_eq!(code, 2);
    }

    use crate::compress::ZstdCompressStage;
    use crate::pipeline::stage::StageStack;
    use crate::storage::BoxAsyncRead;
    use tokio::io::AsyncReadExt;

    /// mongodump archive 포맷 매직(스파이크 §2) — 평문 산출물 식별 시그니처.
    /// 압축·암호화 후 저장 바이트에 이 시퀀스가 남아 있으면 평문 누출이다(SC3).
    const ARCHIVE_MAGIC: &[u8] = &[0x6d, 0xe2, 0x99, 0x81];

    /// SC3 — 기본 경로(compress → encrypt) 산출물에 평문 시그니처(archive 매직·평문 마커)가
    /// 없어야 한다(산출물 평문 아님 검증, R10 AC / DoD).
    #[tokio::test]
    async fn default_path_output_has_no_plaintext_signature() {
        // dump archive를 모사한 평문: 매직 + 식별 가능한 평문 마커 + 반복 데이터.
        let mut plaintext = Vec::new();
        plaintext.extend_from_slice(ARCHIVE_MAGIC);
        plaintext.extend_from_slice(b"PLAINTEXT_MARKER_admin.users");
        plaintext.extend((0..40_000u32).map(|i| (i % 64) as u8));

        // compress → encrypt 스택 구성(PRD §8.4 순서). age 기본 경로.
        let id = ::age::x25519::Identity::generate();
        let mut stack = StageStack::new();
        stack
            .push(Box::new(ZstdCompressStage::new(10)))
            .push(Box::new(AgeEncryptStage::from_recipient(id.to_public())));
        // 순서 단위 검증도 겸한다(compress 먼저, encrypt 나중).
        assert_eq!(stack.stage_names(), vec!["zstd", "age"]);

        let source: BoxAsyncRead = Box::pin(std::io::Cursor::new(plaintext.clone()));
        let mut output = Vec::new();
        stack.apply(source).read_to_end(&mut output).await.unwrap();

        // 산출물에 archive 매직·평문 마커가 없어야 한다.
        assert!(
            !contains_subslice(&output, ARCHIVE_MAGIC),
            "암호화 산출물에 archive 매직이 남아 있음(평문 누출)"
        );
        assert!(
            !contains_subslice(&output, b"PLAINTEXT_MARKER_admin.users"),
            "암호화 산출물에 평문 마커가 남아 있음(평문 누출)"
        );
        // 산출물 자체는 원본과 완전히 달라야 한다.
        assert_ne!(output, plaintext);
    }

    /// 부분 슬라이스 포함 여부.
    fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    /// `n`바이트를 흘린 뒤 io::Error를 반환하는 테스트용 reader(펌프 중간 실패 모사).
    struct FailingReader {
        remaining: usize,
    }

    impl AsyncRead for FailingReader {
        fn poll_read(
            mut self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.remaining == 0 {
                return Poll::Ready(Err(std::io::Error::other("주입된 입력 실패")));
            }
            let n = self.remaining.min(buf.remaining()).min(4096);
            buf.initialize_unfilled_to(n)
                .iter_mut()
                .for_each(|b| *b = 0xAB);
            buf.advance(n);
            self.remaining -= n;
            Poll::Ready(Ok(()))
        }
    }

    /// C1 — 암호화 펌프가 입력 도중 실패하면, 감싼 reader는 **EOF가 아니라 io::Error**를
    /// surface해야 한다(백업이 잘린 산출물을 정상 종료로 오인 → Complete 오기록하는 것 방지).
    #[tokio::test]
    async fn encrypt_pump_failure_surfaces_as_error_age() {
        use tokio::io::AsyncReadExt;
        let id = ::age::x25519::Identity::generate();
        let stage = Box::new(AgeEncryptStage::from_recipient(id.to_public()));
        let input: BoxAsyncRead = Box::pin(FailingReader { remaining: 100_000 });
        let mut out = Vec::new();
        let result = stage.wrap(input).read_to_end(&mut out).await;
        assert!(
            result.is_err(),
            "펌프 실패가 EOF로 삼켜짐 — 잘린 산출물이 정상 종료로 오인됨(C1)"
        );
    }

    /// C1 — AES-GCM 암호화 펌프도 동일하게 실패를 surface해야 한다.
    #[tokio::test]
    async fn encrypt_pump_failure_surfaces_as_error_aes() {
        use tokio::io::AsyncReadExt;
        let stage = Box::new(AesGcmEncryptStage::from_key([3u8; 32]));
        let input: BoxAsyncRead = Box::pin(FailingReader { remaining: 100_000 });
        let mut out = Vec::new();
        let result = stage.wrap(input).read_to_end(&mut out).await;
        assert!(
            result.is_err(),
            "AES 펌프 실패가 EOF로 삼켜짐(C1) — 잘린 산출물이 정상 종료로 오인됨"
        );
    }
}
