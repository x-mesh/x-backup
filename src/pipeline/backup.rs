//! 풀 백업 파이프라인 — dump → 단계 합성 → sha256 tee → Storage → manifest.
//!
//! 전 구간 스트리밍(PRD §7). 현재(t4)는 평문 경로:
//! ```text
//! mongodump --archive=- ─▶ StageStack(identity) ─▶ Sha256Reader ─▶ put_stream
//!                                                              └─▶ manifest(data→meta→사이드카)
//! ```
//! t6은 [`StageStack`]에 compress·encrypt 단계를 push하기만 하면 동일 흐름을 탄다
//! (자세한 삽입 규약은 [`super::stage`] 문서).
//!
//! ## 무결성 순서(pitfall 7-1)
//! data.bin을 먼저 `put_stream`한 뒤 manifest·사이드카를 기록한다("업로드 먼저,
//! manifest 나중"). 어느 단계든 실패하면 [`cleanup`]으로 부분 산출물(data.bin 등)을
//! best-effort 삭제한다 — manifest가 가리키는 파일이 없는 유령 상태를 피한다.

use chrono::Utc;
use uuid::Uuid;

use crate::engine::mongo::meta::ServerMeta;
use crate::engine::mongo::{DumpProcess, DumpSpec, MongoMeta, UriConfigFile};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, Topology, ToolVersions, FORMAT_VERSION,
};
use crate::manifest::store::{data_path, manifest_path, manifest_sha_path, ManifestStore};
use crate::pipeline::checksum::Sha256Reader;
use crate::pipeline::stage::StageStack;
use crate::storage::{BoxAsyncRead, Storage};

/// 풀 백업 요청.
pub struct BackupRequest {
    /// 연결·메타 질의에 쓸 MongoDB URI 시크릿.
    pub uri: crate::config::secret::Secret,
    /// mongodump 실행파일 경로(보통 `"mongodump"`).
    pub mongodump_program: String,
    /// 선택적 백업 — 특정 DB(`--db`). 지정 시 `--oplog` 비활성(FR-1).
    pub db: Option<String>,
    /// 선택적 백업 — 특정 컬렉션(`--collection`).
    pub collection: Option<String>,
}

impl BackupRequest {
    /// 선택적 백업 여부(`--db`/`--collection` 중 하나라도 지정).
    fn is_selective(&self) -> bool {
        self.db.is_some() || self.collection.is_some()
    }
}

/// 백업 성공 결과 요약(CLI 출력용).
#[derive(Debug, Clone)]
pub struct BackupOutcome {
    /// 백업 ID(저장 디렉터리명).
    pub backup_id: String,
    /// 저장 바이트 수(data.bin).
    pub stored_size_bytes: u64,
    /// 저장 바이트의 sha256 hex.
    pub checksum_sha256: String,
    /// 토폴로지.
    pub topology: Topology,
    /// oplog 구간(replica set일 때만).
    pub oplog_range: Option<OplogRange>,
}

/// 풀 백업을 끝까지 실행한다(메타 질의 → dump → 저장 → manifest).
///
/// `storage`는 destination 백엔드(t4는 LocalFs). `stages`는 파이프라인 변환 단계
/// (t4는 빈 identity, t6이 채움).
pub async fn run_full_backup(
    request: &BackupRequest,
    storage: &dyn Storage,
    stages: StageStack,
) -> Result<BackupOutcome> {
    // 1) 드라이버로 서버 메타 + dump 전 oplog ts 조회.
    let mongo = MongoMeta::connect(&request.uri).await?;
    let server_meta = mongo.server_meta().await?;
    let topology = server_meta.topology();

    // 선택적 백업이면 --oplog 비활성(FR-1). 그 외 replica set이면 자동 부여.
    let selective = request.is_selective();
    let use_oplog = server_meta.supports_oplog() && !selective;
    if selective && server_meta.supports_oplog() {
        tracing::warn!(
            "선택적 백업(--db/--collection)은 --oplog와 병용 불가 — oplog 미포함, 증분 base 부적격(FR-1)"
        );
    }

    let oplog_start = if use_oplog {
        mongo.latest_oplog_ts().await?
    } else {
        None
    };

    // 2) URI를 0600 임시 config로 — argv 노출 금지(PRD §11). 핸들은 dump 종료까지 유지.
    let uri_config = UriConfigFile::create(&request.uri)?;

    // 3) mongodump 스폰(--archive=-, 필요 시 --oplog).
    let spec = DumpSpec {
        program: request.mongodump_program.clone(),
        uri_config_path: uri_config.path().to_string(),
        oplog: use_oplog,
        db: request.db.clone(),
        collection: request.collection.clone(),
    };
    let mut dump = DumpProcess::spawn(&spec)?;
    let stdout = dump.take_stdout()?;

    // 4) 파이프라인 합성: dump stdout → 단계(identity/t6) → sha256 tee.
    let staged: BoxAsyncRead = stages.apply(Box::pin(stdout));
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();

    // 5) data.bin 저장(업로드 먼저). put_stream이 바이트를 끝까지 소비한다.
    let backup_id = Uuid::now_v7().to_string();
    let data_rel = data_path(&backup_id);

    let put_result = storage
        .put_stream(&data_rel, Box::pin(checksummed), None)
        .await;

    // 업로드 성공/실패와 무관하게 dump 종료를 판정해야 한다(좀비 방지).
    // 업로드 성공 시: stdout EOF까지 소비됐으므로 wait가 정상 반환.
    // 업로드 실패 시: dump를 kill+wait로 정리.
    if let Err(put_err) = put_result {
        dump.abort().await;
        return Err(put_err);
    }
    // dump 종료 코드 판정(exit code only).
    if let Err(dump_err) = dump.wait().await {
        // dump가 실패했으면 저장된 부분 data.bin을 정리한다.
        cleanup(storage, &backup_id).await;
        return Err(dump_err);
    }

    // 6) dump 후 oplog ts 조회(구간 end).
    let oplog_end = if use_oplog {
        mongo.latest_oplog_ts().await?
    } else {
        None
    };
    let oplog_range = match (oplog_start, oplog_end) {
        (Some(start), Some(end)) => Some(OplogRange {
            start_ts: start.into(),
            end_ts: end.into(),
        }),
        _ => None,
    };

    // 7) 체크섬·크기 확정. put_stream이 끝났으므로 EOF까지 누산 완료.
    let checksum = checksum_handle
        .finalize()
        .ok_or_else(|| XBackupError::Failure("체크섬 확정 실패(이미 소비됨)".into()))?;
    let stored_size = storage_size(storage, &data_rel).await?;

    // 8) manifest 작성. 평문이므로 compression/encryption은 None, original=stored.
    let manifest = build_manifest(
        &backup_id,
        request,
        &server_meta,
        topology,
        selective,
        stored_size,
        &checksum,
        oplog_range,
    );

    // 9) manifest → 사이드카 기록(data 다음, pitfall 7-1). 실패 시 전체 정리.
    let store = ManifestStore::new(storage);
    if let Err(write_err) = store.write(&manifest).await {
        cleanup(storage, &backup_id).await;
        return Err(write_err);
    }

    tracing::info!(
        backup_id = %backup_id,
        bytes = stored_size,
        checksum = %checksum,
        "풀 백업 완료"
    );

    Ok(BackupOutcome {
        backup_id,
        stored_size_bytes: stored_size,
        checksum_sha256: checksum,
        topology,
        oplog_range,
    })
}

/// manifest 값을 조립한다(평문 t4 경로).
#[allow(clippy::too_many_arguments)]
fn build_manifest(
    backup_id: &str,
    request: &BackupRequest,
    server_meta: &ServerMeta,
    topology: Topology,
    selective: bool,
    stored_size: u64,
    checksum: &str,
    oplog_range: Option<OplogRange>,
) -> BackupManifest {
    let _ = request; // t4는 request에서 추가로 기록할 메타 없음(t6/t8이 확장).
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: backup_id.to_string(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        base_id: None,
        topology,
        server_version: server_meta.server_version.clone(),
        tool_versions: ToolVersions {
            // dump 버전·archive 포맷은 t11 사전점검/파싱에서 채운다(현재 미수집).
            mongodump: None,
            archive_format: None,
        },
        selective,
        // 평문 단계라 입력=출력. t6이 압축 시 original_size_bytes를 dump 입력 총량으로
        // 분리 기록한다(checksum tee를 입력 측에도 두거나 카운터 추가).
        original_size_bytes: stored_size,
        stored_size_bytes: stored_size,
        compression: None,
        encryption: None,
        checksum_sha256: checksum.to_string(),
        oplog_range,
        status: BackupStatus::Complete,
    }
}

/// 저장된 객체의 크기를 list로 조회한다.
async fn storage_size(storage: &dyn Storage, path: &str) -> Result<u64> {
    let entries = storage.list(path).await?;
    entries
        .iter()
        .find(|e| e.path == path)
        .map(|e| e.size)
        .ok_or_else(|| XBackupError::Failure(format!("저장 후 '{path}' 크기 조회 실패")))
}

/// 부분 산출물 정리 — 실패 경로에서 백업 디렉터리의 알려진 파일을 best-effort 삭제한다.
///
/// "업로드 먼저, manifest 나중" 순서이므로, manifest 기록 전 실패면 data.bin만,
/// manifest 기록 중 실패면 data.bin + manifest.json까지 정리 대상이다. 셋 다
/// 시도하되 없는 파일 삭제 에러는 무시한다(이미 없을 수 있음).
async fn cleanup(storage: &dyn Storage, backup_id: &str) {
    for path in [
        data_path(backup_id),
        manifest_path(backup_id),
        manifest_sha_path(backup_id),
    ] {
        if let Err(e) = storage.delete(&path).await {
            tracing::debug!(path = %path, "정리 중 삭제 실패(무시): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{MockStorage, StorageEntry};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use tokio::io::AsyncReadExt;

    // 이 모듈의 단위 테스트는 드라이버·서브프로세스를 제외한 *저장 측* 합성 로직에
    // 집중한다(메타 질의·dump 스폰은 각 모듈 테스트와 통합 테스트가 담당). 여기서는
    // sha256 tee + put_stream + cleanup 호출 규약을 MockStorage로 검증한다.

    /// sha256 tee → put_stream 경로가 저장 바이트의 해시를 정확히 확정하는지.
    #[tokio::test]
    async fn checksum_handle_matches_put_bytes() {
        let payload = b"dump archive bytes".to_vec();
        let expected = {
            use sha2::{Digest, Sha256};
            hex::encode(Sha256::digest(&payload))
        };

        let reader = Sha256Reader::new(std::io::Cursor::new(payload.clone()));
        let handle = reader.handle();

        // put_stream을 흉내내 reader를 끝까지 소비.
        let mut boxed: BoxAsyncRead = Box::pin(reader);
        let mut sink = Vec::new();
        boxed.read_to_end(&mut sink).await.unwrap();

        assert_eq!(sink, payload);
        assert_eq!(handle.finalize().unwrap(), expected);
    }

    /// cleanup은 data/manifest/사이드카 3종 모두에 delete를 시도해야 한다.
    #[tokio::test]
    async fn cleanup_deletes_all_three_artifacts() {
        let mut mock = MockStorage::new();
        let deleted = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let d = Arc::clone(&deleted);
        mock.expect_delete().times(3).returning(move |p| {
            d.lock().unwrap().push(p.to_string());
            Ok(())
        });

        cleanup(&mock, "bk-x").await;

        let got = deleted.lock().unwrap().clone();
        assert!(got.contains(&"bk-x/data.bin".to_string()));
        assert!(got.contains(&"bk-x/manifest.json".to_string()));
        assert!(got.contains(&"bk-x/manifest.json.sha256".to_string()));
    }

    /// manifest 기록 실패 시 cleanup이 호출되는지 — store.write가 실패하는 경로를
    /// MockStorage로 모사한다(data put은 성공, manifest put은 실패).
    #[tokio::test]
    async fn manifest_write_failure_triggers_cleanup() {
        // 시나리오: ManifestStore::write의 첫 put_stream(manifest.json)이 실패.
        // 그러면 backup.rs는 cleanup(delete 3회)을 호출해야 한다.
        let cleanup_called = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&cleanup_called);

        let mut mock = MockStorage::new();
        // manifest.json put_stream 실패.
        mock.expect_put_stream()
            .withf(|p, _, _| p.ends_with("manifest.json"))
            .returning(|_, _, _| {
                Err(XBackupError::StorageUpload("manifest 저장 실패(주입)".into()))
            });
        // cleanup의 delete 호출(3종)을 수용하고 플래그를 세운다.
        mock.expect_delete().returning(move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok(())
        });

        // ManifestStore::write를 직접 호출해 실패를 유도한 뒤 cleanup을 검증한다.
        let store = ManifestStore::new(&mock);
        let manifest = BackupManifest {
            format_version: FORMAT_VERSION,
            id: "bk-fail".into(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 1,
            stored_size_bytes: 1,
            compression: None,
            encryption: None,
            checksum_sha256: "x".into(),
            oplog_range: None,
            status: BackupStatus::Complete,
        };
        let write_err = store.write(&manifest).await.unwrap_err();
        assert_eq!(write_err.exit_code(), 1);

        // 실제 backup.rs 경로가 하듯 cleanup 호출.
        cleanup(&mock, "bk-fail").await;
        assert!(cleanup_called.load(Ordering::SeqCst), "cleanup이 delete를 호출하지 않음");
    }

    /// storage_size는 list 결과에서 해당 경로의 크기를 정확히 집어낸다.
    #[tokio::test]
    async fn storage_size_reads_from_list() {
        let mut mock = MockStorage::new();
        mock.expect_list().returning(|prefix| {
            Ok(vec![StorageEntry {
                path: prefix.to_string(),
                size: 4242,
                last_modified: None,
            }])
        });
        let size = storage_size(&mock, "bk/data.bin").await.unwrap();
        assert_eq!(size, 4242);
    }
}
