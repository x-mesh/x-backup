//! 파이프라인 단계 합성 — t6(압축·암호화)의 삽입 지점(리서치 architecture §3).
//!
//! 백업 파이프라인은 dump stdout부터 Storage 입력까지 일련의 **스트림 변환 단계**로
//! 구성된다. 현재(t4)는 평문이므로 단계가 비어 있고, t6이 compress·encrypt 단계를
//! 이 합성 지점에 끼운다.
//!
//! ## 합성 모델: `Vec<Box<dyn PipelineStage>>`
//! 각 단계는 [`BoxAsyncRead`]를 받아 변환된 [`BoxAsyncRead`]를 돌려주는 어댑터다.
//! [`StageStack`]이 단계들을 **선언 순서대로** 합성한다:
//!
//! ```text
//! dump stdout ─▶ stage[0] ─▶ stage[1] ─▶ … ─▶ (sha256 tee) ─▶ put_stream
//! ```
//!
//! ### t6이 끼우는 방법
//! t6은 `Compress`/`Crypto`를 각각 [`PipelineStage`]로 구현한 뒤,
//! [`StageStack::push`]로 **compress → encrypt 순서**(PRD §8.4 고정)로 쌓기만 하면
//! 된다. 파이프라인(backup.rs)·체크섬·Storage 계층은 한 줄도 바뀌지 않는다 —
//! 단계 목록을 만드는 코드만 t6이 채운다. 순서 규칙(compress 먼저, encrypt 나중)은
//! push 순서로 표현되고, 체크섬은 항상 *마지막 단계의 출력*(= 저장 바이트)에 걸린다
//! (PRD §8.5, 리서치 §3 "체크섬 기준점 = 암호화 후 저장 바이트").
//!
//! ## 동기 변환 + 내부 async I/O
//! [`PipelineStage::wrap`]은 **동기 메서드**다(리서치 architecture §2: Crypto/Compress는
//! 스트림 어댑터를 즉시 반환, 실제 I/O는 어댑터의 `poll_read` 안에서 async). 따라서
//! 단계 합성 자체에는 await가 없고, 데이터가 흐를 때 각 어댑터의 `poll_read`가
//! 자연 백프레셔로 연결된다.

use crate::compress::ZstdDecompressStage;
use crate::crypto::{build_decrypt_stage, DecryptKeySource};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::BackupManifest;
use crate::storage::BoxAsyncRead;

/// 복호화 age identity 파일 경로를 담는 환경변수(복구·verify --deep 키 소스, §8.3/§8.5).
///
/// 백업 호스트는 공개키만 보유하므로(§8.1) 복호화는 개인키를 가진 별도 호스트에서
/// 수행한다. `reverse_stack_for`는 이 env에서 identity 파일 경로를 읽는다(핸들러
/// 시그니처를 바꾸지 않고 키 소스를 주입하기 위한 최소 추상화 — 태스크 지침 2).
pub const ENV_AGE_IDENTITY_FILE: &str = "XB_AGE_IDENTITY_FILE";

/// AES-256-GCM 대칭 키(32바이트 hex)를 담는 환경변수(대안 경로 키 소스, §8.2/§8.3).
pub const ENV_AES_KEY_HEX: &str = "XB_AES_KEY_HEX";

/// 단일 스트림 변환 단계.
///
/// 입력 reader를 소비해 변환된 reader를 반환한다. 압축·암호화처럼 바이트를
/// 변형하는 단계가 이 trait를 구현한다(t6). 단계는 상태를 가질 수 있으므로
/// (압축 레벨, age recipient 등) `self`를 통해 구성값을 보관한다.
pub trait PipelineStage: Send {
    /// `input`을 감싼 변환 reader를 반환한다.
    ///
    /// 동기 호출이며 즉시 어댑터를 돌려준다 — 실제 압축/암호화는 반환된 reader가
    /// 폴링될 때 수행된다.
    fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead;

    /// 로깅·진단용 단계 이름(예: `"zstd"`, `"age"`).
    fn name(&self) -> &'static str;
}

/// 순서가 있는 단계 모음. push한 순서대로 입력에 적용된다.
///
/// t4 기본 구성은 비어 있다(평문 = identity). t6이 compress·encrypt 단계를 push해
/// 동일 [`apply`](Self::apply) 합성을 통과시킨다.
#[derive(Default)]
pub struct StageStack {
    stages: Vec<Box<dyn PipelineStage>>,
}

impl StageStack {
    /// 빈 스택(identity 파이프라인 — 평문)을 만든다.
    pub fn new() -> Self {
        Self::default()
    }

    /// 단계를 끝에 추가한다(later push = 데이터 흐름상 더 나중에 적용).
    ///
    /// t6은 `push(compress)` 후 `push(encrypt)`로 PRD §8.4 순서를 표현한다.
    pub fn push(&mut self, stage: Box<dyn PipelineStage>) -> &mut Self {
        self.stages.push(stage);
        self
    }

    /// 등록된 단계가 없으면 true(평문 파이프라인).
    pub fn is_identity(&self) -> bool {
        self.stages.is_empty()
    }

    /// 합성된 단계 이름 목록(로깅·manifest 진단용).
    pub fn stage_names(&self) -> Vec<&'static str> {
        self.stages.iter().map(|s| s.name()).collect()
    }

    /// 모든 단계를 선언 순서대로 `source`에 적용한 최종 reader를 반환한다.
    ///
    /// 단계가 없으면 `source`를 그대로 돌려준다(identity). 반환된 reader가
    /// sha256 tee로 감싸여 Storage로 흐른다.
    pub fn apply(self, source: BoxAsyncRead) -> BoxAsyncRead {
        let mut reader = source;
        for stage in self.stages {
            reader = stage.wrap(reader);
        }
        reader
    }
}

/// manifest 메타를 보고 **복구용 역방향(reverse) 스택**을 만든다(t5 복구 경로).
///
/// 백업 파이프라인이 `dump → compress → encrypt → storage` 순서였으므로, 복구는
/// 그 역순 `storage → decrypt → decompress → mongorestore`로 변환해야 한다(PRD §7
/// "복구 역방향"). 따라서 이 팩토리가 만드는 스택은 storage에서 읽은 바이트에
/// **먼저 복호화 단계, 그 다음 압축해제 단계**를 적용하도록 push 순서를 잡는다
/// ([`StageStack::apply`]는 push 순서대로 감싸므로 decrypt를 먼저 push한다).
///
/// ## 역순 합성 규칙(PRD §8.4)
/// 백업 순서가 compress→encrypt이므로 역순은 **decrypt → decompress**다. [`StageStack`]은
/// push 순서대로 입력을 감싸므로, storage 바이트에 먼저 decrypt를 push하고 그 다음
/// decompress를 push한다. manifest 메타가:
/// - 둘 다 None → identity 스택(평문 round-trip).
/// - `encryption`만 Some → decrypt 단계만.
/// - `compression`만 Some → decompress 단계만.
/// - 둘 다 Some → decrypt 후 decompress.
///
/// ## 키 소스(§8.3/§8.5)
/// 복호화 키는 환경변수에서 읽는다([`ENV_AGE_IDENTITY_FILE`]/[`ENV_AES_KEY_HEX`]) —
/// 핸들러/파이프라인 시그니처를 바꾸지 않고 개인키를 격리 호스트에서 주입하기 위한
/// 최소 추상화다. 암호화 백업인데 해당 env가 없으면 명확한 Config 에러를 반환한다.
pub fn reverse_stack_for(manifest: &BackupManifest) -> Result<StageStack> {
    let mut stack = StageStack::new();

    // 1) 복호화 단계(역순 첫 단계) — storage 바이트에 가장 먼저 적용.
    if let Some(enc_meta) = &manifest.encryption {
        let key = resolve_decrypt_key(&enc_meta.algorithm)?;
        let decrypt = build_decrypt_stage(enc_meta, &key)?;
        stack.push(decrypt);
    }

    // 2) 압축해제 단계(역순 두 번째) — 복호화된 평문에 적용.
    if let Some(comp_meta) = &manifest.compression {
        match comp_meta.algorithm.as_str() {
            crate::compress::ALGORITHM_ZSTD => {
                stack.push(Box::new(ZstdDecompressStage::new()));
            }
            other => {
                return Err(XBackupError::Failure(crate::tr!("unknown compression algorithm: '{other}' (only zstd is supported) — cannot restore", "알 수 없는 압축 알고리즘: '{other}'(zstd만 지원) — 복구 불가")));
            }
        }
    }

    Ok(stack)
}

/// 암호화 알고리즘에 맞는 복호화 키 소스를 환경변수에서 해석한다(§8.3/§8.5).
fn resolve_decrypt_key(algorithm: &str) -> Result<DecryptKeySource> {
    match algorithm {
        crate::crypto::ALGORITHM_AGE => {
            let path = std::env::var(ENV_AGE_IDENTITY_FILE).map_err(|_| {
                XBackupError::Config(crate::tr!("decrypting an age-encrypted backup requires the private key — set the identity file path in {ENV_AGE_IDENTITY_FILE} (key isolation, §8.5)", "age 암호화 백업의 복호화에는 개인키가 필요합니다 — \
                     {ENV_AGE_IDENTITY_FILE}에 identity 파일 경로를 지정하세요(§8.5 키 격리)"))
            })?;
            Ok(DecryptKeySource::AgeIdentityFile(path))
        }
        crate::crypto::ALGORITHM_AES_GCM => {
            let hex = std::env::var(ENV_AES_KEY_HEX).map_err(|_| {
                XBackupError::Config(crate::tr!("decrypting an aes-256-gcm backup requires the symmetric key — set a 32-byte hex key in {ENV_AES_KEY_HEX}", "aes-256-gcm 백업의 복호화에는 대칭 키가 필요합니다 — \
                     {ENV_AES_KEY_HEX}에 32바이트 hex 키를 지정하세요"))
            })?;
            Ok(DecryptKeySource::AesHexKey(hex))
        }
        other => Err(XBackupError::Config(crate::tr!(
            "unknown encryption algorithm: '{other}' (age | aes-256-gcm) — cannot restore",
            "알 수 없는 암호화 알고리즘: '{other}'(age | aes-256-gcm) — 복구 불가"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use tokio::io::AsyncReadExt;

    /// 키 env(`XB_AGE_IDENTITY_FILE` 등)를 만지는 테스트를 직렬화한다 — process-wide
    /// env는 병렬 테스트 간 공유 상태라 동시 set/remove가 서로 간섭할 수 있다.
    static ENV_GUARD: Mutex<()> = Mutex::new(());

    fn reader_from(data: &[u8]) -> BoxAsyncRead {
        Box::pin(std::io::Cursor::new(data.to_vec()))
    }

    /// 빈 스택은 입력을 변형 없이 통과시킨다(identity = 평문 t4 경로).
    #[tokio::test]
    async fn identity_stack_passes_bytes_through() {
        let stack = StageStack::new();
        assert!(stack.is_identity());
        let mut out = Vec::new();
        stack
            .apply(reader_from(b"plaintext dump bytes"))
            .read_to_end(&mut out)
            .await
            .unwrap();
        assert_eq!(out, b"plaintext dump bytes");
    }

    /// 단계는 push 순서대로 합성된다 — t6의 compress→encrypt 순서 표현을 모사한다.
    /// 여기서는 "각 단계가 접미사를 붙이는" 가짜 변환으로 순서만 검증한다.
    #[tokio::test]
    async fn stages_compose_in_push_order() {
        // 입력 끝에 태그 바이트를 덧붙이는 테스트용 단계.
        struct AppendStage(&'static str);
        impl PipelineStage for AppendStage {
            fn wrap(self: Box<Self>, input: BoxAsyncRead) -> BoxAsyncRead {
                Box::pin(input.chain(std::io::Cursor::new(self.0.as_bytes().to_vec())))
            }
            fn name(&self) -> &'static str {
                "append"
            }
        }

        let mut stack = StageStack::new();
        stack
            .push(Box::new(AppendStage("|first")))
            .push(Box::new(AppendStage("|second")));
        assert_eq!(stack.stage_names(), vec!["append", "append"]);

        let mut out = Vec::new();
        stack
            .apply(reader_from(b"base"))
            .read_to_end(&mut out)
            .await
            .unwrap();
        // first가 먼저 감싸고, second가 그 위를 감싸므로 base|first|second 순.
        assert_eq!(out, b"base|first|second");
    }

    use crate::manifest::schema::{
        BackupManifest, BackupStatus, BackupType, CompressionMeta, EncryptionMeta, Topology,
        FORMAT_VERSION,
    };

    fn manifest_with(
        compression: Option<CompressionMeta>,
        encryption: Option<EncryptionMeta>,
    ) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: "bk-rev".into(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 10,
            stored_size_bytes: 10,
            compression,
            encryption,
            checksum_sha256: "x".into(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            status: BackupStatus::Complete,
        }
    }

    /// 평문 manifest(압축·암호화 메타 모두 None)는 identity 역스택을 만든다(t5 평문 경로).
    #[test]
    fn reverse_stack_for_plaintext_is_identity() {
        let m = manifest_with(None, None);
        let stack = reverse_stack_for(&m).expect("평문은 identity 스택이어야 함");
        assert!(stack.is_identity());
    }

    /// 압축만 있는 메타는 decompress 단계 하나짜리 역스택을 만든다(키 불필요).
    #[test]
    fn reverse_stack_for_compressed_builds_decompress_stage() {
        let m = manifest_with(
            Some(CompressionMeta {
                algorithm: "zstd".into(),
                level: 10,
            }),
            None,
        );
        let stack = reverse_stack_for(&m).expect("압축 백업은 decompress 역스택이어야 함");
        assert_eq!(stack.stage_names(), vec!["zstd"]);
    }

    /// 암호화(age) 백업인데 identity env가 없으면 Config 에러(exit 2)로 키 부재를 알린다.
    #[test]
    fn reverse_stack_for_age_without_identity_env_errors() {
        let _guard = ENV_GUARD.lock().unwrap();
        // 이 테스트가 의존하는 env가 없음을 보장(병렬 테스트 격리를 위해 직접 확인).
        // SAFETY: ENV_GUARD로 직렬화된 구간에서만 env를 만지며, 직후 동기 호출 후 해제한다.
        unsafe {
            std::env::remove_var(super::ENV_AGE_IDENTITY_FILE);
        }
        let m = manifest_with(
            None,
            Some(EncryptionMeta {
                algorithm: "age".into(),
                key_id: None,
            }),
        );
        // StageStack은 Debug가 아니므로 match로 에러를 꺼낸다.
        let err = match reverse_stack_for(&m) {
            Ok(_) => panic!("키 없는 age 복구는 실패해야 함"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.to_string().contains(super::ENV_AGE_IDENTITY_FILE),
            "메시지: {err}"
        );
    }

    /// 암호화 + 압축 둘 다 있으면 역순(decrypt → decompress) 스택을 만든다(키 제공 시).
    #[test]
    fn reverse_stack_for_encrypted_and_compressed_orders_decrypt_then_decompress() {
        let _guard = ENV_GUARD.lock().unwrap();
        // identity 파일을 임시로 만들어 env에 주입한다.
        use age::secrecy::ExposeSecret;
        let id = age::x25519::Identity::generate();
        let dir = tempfile::tempdir().unwrap();
        let id_path = dir.path().join("id.txt");
        std::fs::write(&id_path, id.to_string().expose_secret()).unwrap();
        // SAFETY: 테스트 전용 env 설정 — 직후 동기적으로 reverse_stack_for를 호출하고 해제한다.
        unsafe {
            std::env::set_var(super::ENV_AGE_IDENTITY_FILE, &id_path);
        }

        let m = manifest_with(
            Some(CompressionMeta {
                algorithm: "zstd".into(),
                level: 10,
            }),
            Some(EncryptionMeta {
                algorithm: "age".into(),
                key_id: None,
            }),
        );
        let stack = reverse_stack_for(&m).expect("키가 있으면 역스택 생성");
        // decrypt(age) 먼저, decompress(zstd) 다음 — apply가 push 순서로 감싼다.
        assert_eq!(stack.stage_names(), vec!["age", "zstd"]);

        unsafe {
            std::env::remove_var(super::ENV_AGE_IDENTITY_FILE);
        }
    }

    /// 정·역 full round-trip: compress→encrypt(백업) 후 reverse_stack_for(decrypt→decompress,
    /// env identity)로 원본을 복원한다 — t6 정방향 스택과 역스택이 정확히 짝을 이룬다.
    #[tokio::test]
    async fn forward_then_reverse_round_trip() {
        use crate::compress::ZstdCompressStage;
        use crate::crypto::AgeEncryptStage;

        use age::secrecy::ExposeSecret;
        let id = age::x25519::Identity::generate();
        let dir = tempfile::tempdir().unwrap();
        let id_path = dir.path().join("id.txt");
        std::fs::write(&id_path, id.to_string().expose_secret()).unwrap();

        let payload: Vec<u8> = (0..70_000u32).map(|i| (i % 97) as u8).collect();

        // 백업 정방향: compress → encrypt(recipient 직접 — env 불필요).
        let mut forward = StageStack::new();
        forward
            .push(Box::new(ZstdCompressStage::new(8)))
            .push(Box::new(AgeEncryptStage::from_recipient(id.to_public())));
        let mut stored = Vec::new();
        forward
            .apply(reader_from(&payload))
            .read_to_end(&mut stored)
            .await
            .unwrap();
        assert_ne!(stored, payload);

        // 복구 역방향: manifest 메타 기반 reverse_stack_for(decrypt → decompress).
        let m = manifest_with(
            Some(CompressionMeta {
                algorithm: "zstd".into(),
                level: 8,
            }),
            Some(EncryptionMeta {
                algorithm: "age".into(),
                key_id: None,
            }),
        );
        // env는 reverse_stack_for(동기) 호출 동안만 필요하다 — 가드를 await 이전에 해제해
        // MutexGuard가 await을 가로지르지 않게 한다(clippy await_holding_lock 회피).
        let reverse = {
            let _guard = ENV_GUARD.lock().unwrap();
            // SAFETY: ENV_GUARD로 직렬화된 동기 구간에서만 env를 만지고 즉시 해제한다.
            unsafe {
                std::env::set_var(super::ENV_AGE_IDENTITY_FILE, &id_path);
            }
            let stack = reverse_stack_for(&m).expect("역스택 생성");
            unsafe {
                std::env::remove_var(super::ENV_AGE_IDENTITY_FILE);
            }
            stack
        };

        let mut restored = Vec::new();
        reverse
            .apply(reader_from(&stored))
            .read_to_end(&mut restored)
            .await
            .unwrap();
        assert_eq!(restored, payload, "full round-trip 후 원본 불일치");
    }
}
