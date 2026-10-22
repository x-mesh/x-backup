//! 무결성 검증 파이프라인 — 구조/심층/체인(PRD §FR-7, §8.5).
//!
//! `verify`는 "백업 존재 ≠ 복구 가능"을 메우는 1급 기능이다(PRD 디자인 원칙). 세 층위로
//! 동작한다:
//!
//! 1. **구조 검증(기본):** manifest 사이드카 체크섬으로 manifest 자체 정합을 확인하고,
//!    `data.bin` 스트림의 sha256을 재계산해 `manifest.checksum_sha256`과 대조한다.
//!    체크섬 기준점이 **저장 바이트**(암호화 후)이므로 **키 없이** 백업 호스트에서
//!    동작한다(§8.5 키 격리, 설계 불변). 변조는 [`XBackupError::Failure`](exit 1),
//!    incomplete manifest는 경고(exit 4).
//!
//! 2. **심층 검증(`--deep`):** [`reverse_stack_for`]로 복호화·압축해제 스트림을
//!    **끝까지 디코드 소진**해(mongorestore 없이) 디코드 가능성만 확인한다. 키 env가
//!    없으면 명확한 거부(개인키 보유 호스트 안내, §8.5).
//!
//! 3. **체인 검증(`--chain`):** destination의 모든 manifest를 모아
//!    [`verify_chain`](crate::manifest::chain::verify_chain)으로 base+증분 연속성을
//!    판정한다. 끊어진 지점을 구체적으로 보고한다(PITR 전제, §6.4).
//!
//! ## 빈 증분 슬라이스 계약(t8과의 계약, pitfall 3-2)
//! `oplog_count == Some(0)`인 증분은 변경이 없어 `data.bin`을 업로드하지 않는다
//! (`stored_size_bytes == 0`). verify는 이때 data 파일의 부재를 **정상**으로 취급하고
//! 빈 입력의 sha256(`e3b0…`)을 기대값으로 본다. 즉, `data.bin`을 강제하지 않는다.

use tokio::io::AsyncReadExt;

use crate::error::{Result, XBackupError};
use crate::manifest::chain::{verify_chain, ChainNode, ChainReport};
use crate::manifest::schema::{BackupManifest, BackupStatus};
use crate::manifest::store::{
    data_path, manifest_path, manifest_sha_path, parse_sidecar, sidecar_checksum, ManifestStore,
    MANIFEST_FILE,
};
use crate::pipeline::stage::reverse_stack_for;
use crate::storage::Storage;

/// 빈 입력의 sha256(증분 빈 슬라이스의 기대 체크섬, RFC 6234 테스트 벡터).
const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// 단일 검증 항목 결과(구조/심층). 체인은 [`ChainReport`]로 별도.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// 검증한 백업 ID.
    pub backup_id: String,
    /// 사이드카 체크섬으로 manifest 자체 정합이 확인됐는지.
    pub manifest_sidecar_ok: bool,
    /// data.bin 재계산 sha256이 manifest.checksum_sha256과 일치하는지.
    pub data_checksum_ok: bool,
    /// `--deep` 수행 시 복호화·압축해제 디코드가 끝까지 성공했는지(미수행이면 None).
    pub deep_decode_ok: Option<bool>,
    /// incomplete manifest 등 경고(있으면 exit 4). 비어 있으면 무경고.
    pub warnings: Vec<String>,
    /// 빈 증분 슬라이스(oplog_count==Some(0))로 data 부재가 정상 처리된 경우 true.
    pub empty_slice: bool,
}

impl VerifyReport {
    /// 구조(+심층) 검증이 모두 통과했는지.
    pub fn is_ok(&self) -> bool {
        self.manifest_sidecar_ok && self.data_checksum_ok && self.deep_decode_ok.unwrap_or(true)
    }
}

/// 구조 검증을 수행한다(키 불필요). `deep`이면 복호화·압축해제 디코드까지 확인한다.
///
/// 반환 규칙(호출자가 exit code로 매핑):
/// - 정합 실패(사이드카 불일치·체크섬 불일치·deep 디코드 실패): [`XBackupError::Failure`](exit 1).
/// - deep인데 키 부재: [`XBackupError::Config`](exit 1로 매핑하도록 호출자가 처리하거나
///   여기서는 메시지로 안내) — 본 함수는 `Config`(키 부재는 사용 환경 문제)로 반환하되
///   호출자(verify 핸들러)가 exit 1로 보정한다(§8.5: 키 부재 = 검증 실패와 동급, DoD).
/// - incomplete 등 경고는 에러가 아니라 `warnings`에 담는다(호출자가 exit 4).
pub async fn verify_backup(
    storage: &dyn Storage,
    backup_id: &str,
    deep: bool,
) -> Result<VerifyReport> {
    let store = ManifestStore::new(storage);

    // 1) manifest 로드(읽기 실패 = 검증 불가, exit 1).
    let manifest = store.read(backup_id).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "failed to load the manifest ({backup_id}): {e}",
            "manifest 로드 실패({backup_id}): {e}"
        ))
    })?;

    let mut warnings = Vec::new();

    // 2) manifest 사이드카 체크섬으로 manifest 자체 정합 확인.
    let manifest_sidecar_ok = verify_manifest_sidecar(storage, backup_id).await?;
    if !manifest_sidecar_ok {
        return Err(XBackupError::Failure(crate::tr!("manifest self-integrity check failed ({backup_id}): manifest.json does not match its sidecar checksum (tampered or corrupt)", "manifest 자체 무결성 검증 실패({backup_id}): manifest.json이 \
             사이드카 체크섬과 일치하지 않습니다(변조 또는 손상)")));
    }

    // 3) incomplete 경고(증분 base 부적격·복구 위험).
    if matches!(manifest.status, BackupStatus::Incomplete) {
        warnings.push(format!(
            "백업 '{backup_id}'이 incomplete 상태입니다 — 복구·증분 base로 사용하지 마세요"
        ));
    }

    // 4) 빈 증분 슬라이스 판정(data.bin 부재가 정상).
    let empty_slice = manifest.oplog_count == Some(0);

    // 5) data.bin 스트림 sha256 재계산 vs manifest.checksum_sha256(키 불필요).
    let data_checksum_ok = verify_data_checksum(storage, backup_id, &manifest, empty_slice).await?;
    if !data_checksum_ok {
        return Err(XBackupError::Failure(crate::tr!("data.bin checksum mismatch ({backup_id}): the stored bytes differ from manifest.checksum_sha256 (tampered or corrupt)", "data.bin 체크섬 불일치({backup_id}): 저장 바이트가 manifest.checksum_sha256과 \
             다릅니다(변조 또는 손상)")));
    }

    // 6) 심층 검증(옵션) — 복호화·압축해제 디코드 소진.
    let deep_decode_ok = if deep {
        Some(verify_deep_decode(storage, backup_id, &manifest, empty_slice).await?)
    } else {
        None
    };

    Ok(VerifyReport {
        backup_id: backup_id.to_string(),
        manifest_sidecar_ok,
        data_checksum_ok,
        deep_decode_ok,
        warnings,
        empty_slice,
    })
}

/// manifest.json 바이트의 sha256을 사이드카(`manifest.json.sha256`)와 대조한다.
///
/// 사이드카가 없으면(부분 기록 등) 검증 불가로 false를 반환한다(호출자가 변조로 처리).
async fn verify_manifest_sidecar(storage: &dyn Storage, backup_id: &str) -> Result<bool> {
    let manifest_bytes = read_all(storage, &manifest_path(backup_id)).await?;
    let sidecar_raw = match read_all(storage, &manifest_sha_path(backup_id)).await {
        Ok(b) => b,
        Err(_) => return Ok(false), // 사이드카 부재 — 정합 확인 불가.
    };
    let sidecar = String::from_utf8_lossy(&sidecar_raw);
    let recorded = match parse_sidecar(&sidecar) {
        Some(h) => h,
        None => return Ok(false),
    };
    // 같은 알고리즘으로 현재 manifest 바이트의 사이드카를 재계산해 hex만 비교.
    let expected = parse_sidecar(&sidecar_checksum(&manifest_bytes))
        .expect("재계산 사이드카는 항상 hex를 포함한다");
    Ok(recorded.eq_ignore_ascii_case(&expected))
}

/// data.bin 스트림의 sha256을 재계산해 manifest.checksum_sha256과 대조한다(키 불필요).
///
/// 빈 슬라이스(`empty_slice`)면 data.bin 부재를 정상으로 보고 빈 입력 sha256을 기대값으로
/// 삼는다(t8 계약). data.bin이 있으면 끝까지 스트리밍하며 누산한다.
async fn verify_data_checksum(
    storage: &dyn Storage,
    backup_id: &str,
    manifest: &BackupManifest,
    empty_slice: bool,
) -> Result<bool> {
    let data_rel = data_path(backup_id);
    let reader = match storage.get_stream(&data_rel).await {
        Ok(r) => r,
        Err(_) if empty_slice => {
            // 빈 증분: data.bin이 없는 것이 정상. manifest 체크섬은 빈 입력 해시여야 한다.
            return Ok(manifest.checksum_sha256.eq_ignore_ascii_case(EMPTY_SHA256));
        }
        Err(e) => {
            return Err(XBackupError::Failure(crate::tr!("failed to read data.bin ({backup_id}): {e} — the artifact is missing or unreachable", "data.bin 읽기 실패({backup_id}): {e} — 산출물이 없거나 접근 불가")));
        }
    };

    let computed = stream_sha256(reader).await?;
    Ok(computed.eq_ignore_ascii_case(&manifest.checksum_sha256))
}

/// 복호화·압축해제 스택으로 data.bin을 끝까지 디코드 소진한다(mongorestore 불필요).
///
/// 디코드가 끝까지 성공하면 true. 키 부재는 `reverse_stack_for`가 Config 에러로 알린다
/// (호출자가 §8.5 안내로 보정). 디코드 도중 실패(키 불일치·손상)는 Failure(exit 1).
async fn verify_deep_decode(
    storage: &dyn Storage,
    backup_id: &str,
    manifest: &BackupManifest,
    empty_slice: bool,
) -> Result<bool> {
    // 빈 슬라이스는 data.bin이 없으므로 디코드 대상이 없다 — 자명히 통과.
    if empty_slice && storage.get_stream(&data_path(backup_id)).await.is_err() {
        return Ok(true);
    }

    // 역스택 구성(키 env에서 해석; 평문이면 identity). 키 부재 시 여기서 Config 에러.
    let stages = reverse_stack_for(manifest)?;
    let raw = storage
        .get_stream(&data_path(backup_id))
        .await
        .map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to read data.bin ({backup_id}): {e}",
                "data.bin 읽기 실패({backup_id}): {e}"
            ))
        })?;
    let mut decoded = stages.apply(raw);

    // 끝까지 읽어 디코드 가능성만 확인한다(평문은 그대로 흘려 보냄). 메모리에 쌓지 않고
    // 고정 버퍼로 소진해 대용량 산출물도 일정 메모리로 검증한다.
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match decoded.read(&mut buf).await {
            Ok(0) => break, // EOF — 끝까지 디코드 성공.
            Ok(_) => continue,
            Err(e) => {
                return Err(XBackupError::Failure(crate::tr!("deep verification failed ({backup_id}): error while decrypting/decompressing — {e} (wrong key or corrupt artifact)", "심층 검증 실패({backup_id}): 복호화·압축해제 디코드 중 오류 — {e} \
                     (키 불일치 또는 산출물 손상)")));
            }
        }
    }
    Ok(true)
}

/// reader를 끝까지 소비하며 sha256 hex를 계산한다(고정 버퍼 스트리밍).
async fn stream_sha256(mut reader: crate::storage::BoxAsyncRead) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf).await.map_err(|e| {
            XBackupError::Failure(crate::tr!(
                "failed to read the data.bin stream: {e}",
                "data.bin 스트림 읽기 실패: {e}"
            ))
        })?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// destination의 모든 manifest를 모아 `target_id` 체인 연속성을 판정한다(PITR 전제).
///
/// 누락 base·ts 불연속·incomplete·selective base 등 끊어진 지점을 [`ChainReport`]로
/// 구체적으로 보고한다. manifest 읽기 실패(부분 산출물)는 노드에서 제외하고 디버그
/// 로그만 남긴다 — 누락은 chain 판정이 MissingBase 등으로 드러낸다.
pub async fn verify_chain_for(storage: &dyn Storage, target_id: &str) -> Result<ChainReport> {
    let store = ManifestStore::new(storage);
    let ids = collect_manifest_ids(storage).await?;

    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        match store.read(id).await {
            Ok(m) => nodes.push(ChainNode::from_manifest(&m)),
            Err(e) => {
                tracing::debug!(id = %id, "manifest 읽기 실패(체인 노드에서 제외): {e}");
            }
        }
    }

    Ok(verify_chain(&nodes, target_id))
}

/// destination에서 `<id>/manifest.json` 경로를 가진 백업 ID를 수집한다(중복 제거·정렬).
pub async fn collect_manifest_ids(storage: &dyn Storage) -> Result<Vec<String>> {
    let entries = storage.list("").await?;
    let suffix = format!("/{MANIFEST_FILE}");
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_suffix(&suffix)
                .filter(|id| !id.is_empty() && !id.contains('/'))
                .map(|id| id.to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();
    Ok(ids)
}

/// Storage 경로의 바이트를 모두 읽는다(내부 헬퍼).
async fn read_all(storage: &dyn Storage, path: &str) -> Result<Vec<u8>> {
    let mut reader = storage.get_stream(path).await?;
    let mut buf = Vec::new();
    reader.read_to_end(&mut buf).await.map_err(|e| {
        XBackupError::Failure(crate::tr!(
            "'{path}': failed to read: {e}",
            "'{path}' 읽기 실패: {e}"
        ))
    })?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        BackupManifest, BackupType, EncryptionMeta, OplogRange, OplogTimestamp, Topology,
        FORMAT_VERSION,
    };
    use crate::storage::{BoxAsyncRead, LocalFs};

    fn manifest(id: &str, checksum: &str) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 0,
            stored_size_bytes: 0,
            compression: None,
            encryption: None,
            checksum_sha256: checksum.to_string(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            status: BackupStatus::Complete,
        }
    }

    fn sha256_hex(data: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(data))
    }

    /// 평문 data.bin + 정확한 체크섬으로 백업을 만든다(테스트 산출물 조립).
    async fn seed(fs: &LocalFs, m: &BackupManifest, data: &[u8]) {
        let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(data.to_vec()));
        fs.put_stream(&data_path(&m.id), reader, Some(data.len() as u64))
            .await
            .unwrap();
        ManifestStore::new(fs).write(m).await.unwrap();
    }

    /// 기본(구조) 검증은 키 없이 통과한다(평문 백업).
    #[tokio::test]
    async fn structural_verify_passes_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"plaintext archive bytes";
        let m = manifest("bk-ok", &sha256_hex(payload));
        seed(&fs, &m, payload).await;

        let report = verify_backup(&fs, "bk-ok", false).await.unwrap();
        assert!(report.is_ok(), "구조 검증 실패: {report:?}");
        assert!(report.manifest_sidecar_ok);
        assert!(report.data_checksum_ok);
        assert!(report.deep_decode_ok.is_none());
        assert!(report.warnings.is_empty());
    }

    /// 변조 산출물(1바이트 flip)은 체크섬 불일치로 실패한다(exit 1).
    #[tokio::test]
    async fn tampered_data_fails_verify() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"original archive bytes";
        // manifest는 원본 체크섬을 기록하지만, data.bin은 변조해서 저장한다.
        let m = manifest("bk-tamper", &sha256_hex(payload));
        let mut tampered = payload.to_vec();
        tampered[0] ^= 0x01; // 1바이트 flip.
        seed(&fs, &m, &tampered).await;

        let err = verify_backup(&fs, "bk-tamper", false).await.unwrap_err();
        assert_eq!(err.exit_code(), 1, "변조는 exit 1이어야 함: {err}");
        let msg = err.to_string();
        assert!(
            msg.contains("checksum") || msg.contains("체크섬"),
            "message: {err}"
        );
    }

    /// manifest 변조(사이드카 불일치)도 실패한다(exit 1).
    #[tokio::test]
    async fn tampered_manifest_fails_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"data";
        let m = manifest("bk-mt", &sha256_hex(payload));
        seed(&fs, &m, payload).await;

        // manifest.json을 사이드카와 어긋나게 덮어쓴다(사이드카는 그대로 둠).
        let bad: BoxAsyncRead = Box::pin(std::io::Cursor::new(br#"{"tampered":true}"#.to_vec()));
        fs.put_stream(&manifest_path("bk-mt"), bad, None)
            .await
            .unwrap();

        let err = verify_backup(&fs, "bk-mt", false).await.unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("manifest"), "메시지: {err}");
    }

    /// incomplete manifest는 경고를 동반하지만 구조 검증 자체는 통과한다(exit 4 신호).
    #[tokio::test]
    async fn incomplete_manifest_warns() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"partial";
        let mut m = manifest("bk-inc", &sha256_hex(payload));
        m.status = BackupStatus::Incomplete;
        seed(&fs, &m, payload).await;

        let report = verify_backup(&fs, "bk-inc", false).await.unwrap();
        assert!(report.is_ok());
        assert!(!report.warnings.is_empty(), "incomplete는 경고가 있어야 함");
    }

    /// 빈 증분 슬라이스(oplog_count==0, data.bin 부재)는 정상으로 통과한다(t8 계약).
    #[tokio::test]
    async fn empty_slice_without_data_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // 빈 슬라이스: checksum = 빈 입력 해시, oplog_count = Some(0), data.bin 미기록.
        let mut m = manifest("bk-empty", EMPTY_SHA256);
        m.backup_type = BackupType::Incremental;
        m.base_id = Some("base".into());
        m.oplog_count = Some(0);
        m.oplog_range = Some(OplogRange {
            start_ts: OplogTimestamp::new(100, 1),
            end_ts: OplogTimestamp::new(100, 1),
        });
        // data.bin은 일부러 기록하지 않는다.
        ManifestStore::new(&fs).write(&m).await.unwrap();

        let report = verify_backup(&fs, "bk-empty", false).await.unwrap();
        assert!(report.is_ok(), "빈 슬라이스 검증 실패: {report:?}");
        assert!(report.empty_slice);
    }

    /// 빈 슬라이스 --deep도 디코드 대상이 없어 통과한다.
    #[tokio::test]
    async fn empty_slice_deep_is_ok() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let mut m = manifest("bk-empty-deep", EMPTY_SHA256);
        m.backup_type = BackupType::Incremental;
        m.base_id = Some("base".into());
        m.oplog_count = Some(0);
        ManifestStore::new(&fs).write(&m).await.unwrap();

        let report = verify_backup(&fs, "bk-empty-deep", true).await.unwrap();
        assert!(report.is_ok());
        assert_eq!(report.deep_decode_ok, Some(true));
    }

    /// 평문 백업 --deep: identity 역스택으로 끝까지 디코드 성공(키 불필요).
    #[tokio::test]
    async fn deep_plaintext_decodes_without_key() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"plaintext that decodes as identity";
        let m = manifest("bk-deep-plain", &sha256_hex(payload));
        seed(&fs, &m, payload).await;

        let report = verify_backup(&fs, "bk-deep-plain", true).await.unwrap();
        assert!(report.is_ok());
        assert_eq!(report.deep_decode_ok, Some(true));
    }

    /// 암호화 백업 --deep인데 키 env가 없으면 거부한다(exit 2 — 키 부재).
    /// (verify 핸들러가 이를 exit 1로 보정한다; 여기서는 Config 에러임을 확인.)
    #[tokio::test]
    async fn deep_encrypted_without_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // age 암호화로 표시한 manifest(실제 암호문 불필요 — 키 해석에서 먼저 막힘).
        let mut m = manifest("bk-enc", &sha256_hex(b"ciphertext placeholder"));
        m.encryption = Some(EncryptionMeta {
            algorithm: "age".into(),
            key_id: Some("age1xxx".into()),
        });
        // 체크섬이 맞아야 deep 단계까지 도달하므로 실제 저장 바이트로 맞춘다.
        let ciphertext = b"ciphertext placeholder";
        let mut m2 = m.clone();
        m2.checksum_sha256 = sha256_hex(ciphertext);
        seed(&fs, &m2, ciphertext).await;

        // 키 env가 없음을 보장하고 deep 검증을 시도한다.
        // SAFETY: 테스트에서 직전 동기적으로 제거하고 곧바로 호출한다.
        unsafe {
            std::env::remove_var(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE);
        }
        let err = verify_backup(&fs, "bk-enc", true).await.unwrap_err();
        // reverse_stack_for의 키 부재는 Config(exit 2). 핸들러가 exit 1로 보정한다.
        assert_eq!(err.exit_code(), 2, "키 부재는 Config 에러: {err}");
        assert!(
            err.to_string()
                .contains(crate::pipeline::stage::ENV_AGE_IDENTITY_FILE),
            "키 안내 메시지 누락: {err}"
        );
    }

    /// 체인 검증: 연속 체인은 ok, 불연속은 끊김 보고.
    #[tokio::test]
    async fn chain_verify_collects_and_reports() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        // base(full) + incr 두 개를 manifest로 기록(data는 검증과 무관하므로 최소).
        let base = {
            let mut m = manifest("base", EMPTY_SHA256);
            m.oplog_range = Some(OplogRange {
                start_ts: OplogTimestamp::new(100, 1),
                end_ts: OplogTimestamp::new(100, 1),
            });
            m
        };
        let i1 = {
            let mut m = manifest("i1", EMPTY_SHA256);
            m.backup_type = BackupType::Incremental;
            m.base_id = Some("base".into());
            m.oplog_range = Some(OplogRange {
                start_ts: OplogTimestamp::new(100, 1),
                end_ts: OplogTimestamp::new(150, 2),
            });
            m
        };
        let i2_gap = {
            let mut m = manifest("i2", EMPTY_SHA256);
            m.backup_type = BackupType::Incremental;
            m.base_id = Some("base".into());
            // gap: 150,2가 아니라 160,0에서 시작.
            m.oplog_range = Some(OplogRange {
                start_ts: OplogTimestamp::new(160, 0),
                end_ts: OplogTimestamp::new(200, 3),
            });
            m
        };
        for m in [&base, &i1, &i2_gap] {
            ManifestStore::new(&fs).write(m).await.unwrap();
        }

        let report = verify_chain_for(&fs, "i2").await.unwrap();
        assert_eq!(report.base_id, "base");
        assert!(!report.is_continuous(), "gap이 있는데 연속으로 판정됨");
        assert_eq!(report.incremental_ids, vec!["i1", "i2"]);
    }

    /// collect_manifest_ids는 manifest.json이 있는 디렉터리만 수집한다.
    #[tokio::test]
    async fn collect_ids_finds_manifests_only() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let m = manifest("bk1", EMPTY_SHA256);
        ManifestStore::new(&fs).write(&m).await.unwrap();
        // manifest 없는 data만 있는 유령 디렉터리.
        let ghost: BoxAsyncRead = Box::pin(std::io::Cursor::new(b"x".to_vec()));
        fs.put_stream("ghost/data.bin", ghost, None).await.unwrap();

        let ids = collect_manifest_ids(&fs).await.unwrap();
        assert_eq!(ids, vec!["bk1"]);
    }
}
