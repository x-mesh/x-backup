//! Manifest 저장·조회 — 레이아웃과 기록 순서(PRD §FR-7, 리서치 §7, pitfall 7-1).
//!
//! ## 저장 레이아웃
//! ```text
//! <backup-id>/data.bin             — 파이프라인 출력(평문/압축/암호문)
//! <backup-id>/manifest.json        — BackupManifest(평문 JSON)
//! <backup-id>/manifest.json.sha256 — manifest.json 바이트의 sha256 hex(사이드카)
//! ```
//! 경로는 [`Storage`] 루트 기준 상대 경로다(t3 계약). S3 prefix 등은 백엔드가 합성한다.
//!
//! ## 기록 순서 — data → manifest → 사이드카 (pitfall 7-1)
//! **업로드 먼저, manifest 나중.** manifest에 적힌 data.bin은 반드시 먼저 존재해야
//! 한다(유령 참조 방지). data → manifest → 사이드카 순으로 기록하며, 이 순서는
//! [`write`](ManifestStore::write)가 보장한다. 실제 파이프라인(backup.rs)은 data를
//! 먼저 `put_stream`한 뒤 이 함수로 manifest·사이드카를 기록한다.
//!
//! ## 원자성 위임
//! 로컬 파일의 `.tmp + rename` 원자 교체는 [`Storage`] 계층(LocalFs/object_store)에
//! 위임한다(t3). 이 모듈은 *기록 순서*만 책임진다.

use crate::error::{Result, XBackupError};
use crate::manifest::schema::BackupManifest;
use crate::storage::{BoxAsyncRead, Storage};

/// 백업 산출물 파일명.
pub const DATA_FILE: &str = "data.bin";
/// manifest 파일명.
pub const MANIFEST_FILE: &str = "manifest.json";
/// manifest 사이드카 체크섬 파일명.
pub const MANIFEST_SHA_FILE: &str = "manifest.json.sha256";

/// `<backup-id>/data.bin` 상대 경로.
pub fn data_path(backup_id: &str) -> String {
    format!("{backup_id}/{DATA_FILE}")
}

/// `<backup-id>/manifest.json` 상대 경로.
pub fn manifest_path(backup_id: &str) -> String {
    format!("{backup_id}/{MANIFEST_FILE}")
}

/// `<backup-id>/manifest.json.sha256` 상대 경로.
pub fn manifest_sha_path(backup_id: &str) -> String {
    format!("{backup_id}/{MANIFEST_SHA_FILE}")
}

/// [`Storage`] 위에 manifest를 기록·조회하는 얇은 헬퍼.
pub struct ManifestStore<'a> {
    storage: &'a dyn Storage,
}

impl<'a> ManifestStore<'a> {
    /// 주어진 백엔드 위에서 동작하는 store를 만든다.
    pub fn new(storage: &'a dyn Storage) -> Self {
        Self { storage }
    }

    /// manifest.json과 사이드카 체크섬을 **순서대로** 기록한다.
    ///
    /// 호출 전 `data.bin`이 이미 저장되어 있어야 한다(pitfall 7-1). 이 함수는
    /// manifest → 사이드카 순으로만 기록한다.
    pub async fn write(&self, manifest: &BackupManifest) -> Result<()> {
        let backup_id = &manifest.id;

        // manifest를 직렬화한다. 사이드카는 *직렬화된 바이트*의 sha256이어야
        // 하므로(파일을 그대로 검증), 같은 바이트로 양쪽을 만든다.
        let bytes = serde_json::to_vec_pretty(manifest)
            .map_err(|e| XBackupError::Failure(format!("manifest 직렬화 실패: {e}")))?;
        let sidecar = sidecar_checksum(&bytes);

        // 1) manifest.json
        self.storage
            .put_stream(
                &manifest_path(backup_id),
                boxed_reader(bytes.clone()),
                Some(bytes.len() as u64),
            )
            .await?;

        // 2) manifest.json.sha256 (사이드카는 manifest 다음 — 사이드카가 가리키는
        //    manifest는 반드시 먼저 존재해야 한다)
        let sidecar_bytes = sidecar.into_bytes();
        self.storage
            .put_stream(
                &manifest_sha_path(backup_id),
                boxed_reader(sidecar_bytes.clone()),
                Some(sidecar_bytes.len() as u64),
            )
            .await?;

        Ok(())
    }

    /// manifest.json을 읽어 역직렬화한다(list/verify 경로).
    pub async fn read(&self, backup_id: &str) -> Result<BackupManifest> {
        let bytes = read_all(self.storage, &manifest_path(backup_id)).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| XBackupError::Failure(format!("manifest 파싱 실패({backup_id}): {e}")))
    }
}

/// manifest 바이트의 사이드카 체크섬 문자열을 만든다.
///
/// 포맷: `<hex>  manifest.json\n` — `sha256sum` 호환 형식이라 외부 도구로도 검증 가능.
pub fn sidecar_checksum(manifest_bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hex = hex::encode(Sha256::digest(manifest_bytes));
    format!("{hex}  {MANIFEST_FILE}\n")
}

/// 사이드카 문자열에서 hex 다이제스트만 추출한다(verify용).
pub fn parse_sidecar(sidecar: &str) -> Option<String> {
    sidecar.split_whitespace().next().map(|s| s.to_string())
}

/// `Vec<u8>`를 [`BoxAsyncRead`]로 감싼다.
fn boxed_reader(bytes: Vec<u8>) -> BoxAsyncRead {
    Box::pin(std::io::Cursor::new(bytes))
}

/// Storage 경로의 바이트를 모두 읽어 들인다.
async fn read_all(storage: &dyn Storage, path: &str) -> Result<Vec<u8>> {
    use tokio::io::AsyncReadExt;
    let mut reader = storage.get_stream(path).await?;
    let mut buf = Vec::new();
    reader
        .read_to_end(&mut buf)
        .await
        .map_err(|e| XBackupError::StorageDownload(format!("'{path}' 읽기 실패: {e}")))?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{BackupStatus, BackupType, Topology, FORMAT_VERSION};
    use crate::storage::LocalFs;

    fn sample(id: &str) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: "2026-06-12T13:00:00Z".to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".to_string(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 3,
            stored_size_bytes: 3,
            compression: None,
            encryption: None,
            checksum_sha256: "abc".to_string(),
            oplog_range: None,
            oplog_count: None,
            promoted_from_gap: false,
            mysql_binlog: None,
            status: BackupStatus::Complete,
        }
    }

    #[test]
    fn layout_paths_are_under_backup_id() {
        assert_eq!(data_path("bk1"), "bk1/data.bin");
        assert_eq!(manifest_path("bk1"), "bk1/manifest.json");
        assert_eq!(manifest_sha_path("bk1"), "bk1/manifest.json.sha256");
    }

    #[test]
    fn sidecar_is_sha256_of_manifest_bytes() {
        let bytes = b"{\"hello\":1}";
        let sidecar = sidecar_checksum(bytes);
        let hex = parse_sidecar(&sidecar).unwrap();
        // sha256sum 형식: hex 두 칸 파일명.
        assert!(sidecar.ends_with("  manifest.json\n"));
        assert_eq!(hex.len(), 64);
        // 알려진 일괄 계산과 일치.
        use sha2::{Digest, Sha256};
        assert_eq!(hex, hex::encode(Sha256::digest(bytes)));
    }

    /// write → read round-trip + 사이드카가 manifest 바이트와 정합한다.
    #[tokio::test]
    async fn write_then_read_round_trip_with_valid_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let store = ManifestStore::new(&fs);

        let m = sample("bk-rt");
        store.write(&m).await.unwrap();

        // manifest 복원.
        let back = store.read("bk-rt").await.unwrap();
        assert_eq!(back, m);

        // 사이드카가 실제 manifest.json 바이트의 sha256과 일치하는지 검증.
        use tokio::io::AsyncReadExt;
        let mut manifest_bytes = Vec::new();
        fs.get_stream("bk-rt/manifest.json")
            .await
            .unwrap()
            .read_to_end(&mut manifest_bytes)
            .await
            .unwrap();
        let mut sidecar = String::new();
        fs.get_stream("bk-rt/manifest.json.sha256")
            .await
            .unwrap()
            .read_to_string(&mut sidecar)
            .await
            .unwrap();
        let expected = parse_sidecar(&sidecar_checksum(&manifest_bytes)).unwrap();
        assert_eq!(parse_sidecar(&sidecar).unwrap(), expected);
    }
}
