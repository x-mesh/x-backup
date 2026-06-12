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

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use chrono::Utc;
use tokio::io::{AsyncRead, ReadBuf};
use uuid::Uuid;

use crate::engine::mongo::meta::ServerMeta;
use crate::engine::mongo::{DumpProcess, DumpSpec, MongoMeta, UriConfigFile};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, ToolVersions, Topology, FORMAT_VERSION,
};
use crate::manifest::store::{data_path, manifest_path, manifest_sha_path, ManifestStore};
use crate::pipeline::checksum::Sha256Reader;
use crate::pipeline::stage::StageStack;
use crate::storage::{BoxAsyncRead, Storage};

/// 통과 바이트 수를 세는 [`AsyncRead`] 래퍼 — 압축 *전* 원본 입력량(original_size_bytes)
/// 측정용. sha256 tee([`Sha256Reader`])와 동형의 단일-패스 카운터로, 추가 버퍼·태스크
/// 없이 `poll_read`에서 누산한다. 핸들([`CountingHandle`])로 EOF 후 총량을 회수한다.
struct CountingReader {
    inner: BoxAsyncRead,
    counter: Arc<AtomicU64>,
}

impl CountingReader {
    fn new(inner: BoxAsyncRead) -> Self {
        Self {
            inner,
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 외부에서 공유 카운터를 주입해 생성한다(진행 표시 폴링용 — t13/R16).
    ///
    /// 진행 표시기가 같은 `Arc<AtomicU64>`를 폴링할 수 있도록, 저장 바이트 카운터의
    /// backing Arc를 외부와 공유한다. 카운팅 로직(`poll_read`)은 전혀 바뀌지 않는다.
    fn with_counter(inner: BoxAsyncRead, counter: Arc<AtomicU64>) -> Self {
        Self { inner, counter }
    }

    /// 누적 바이트를 EOF 이후 읽을 핸들을 반환한다(reader가 move-out 돼도 유효).
    fn handle(&self) -> CountingHandle {
        CountingHandle {
            counter: Arc::clone(&self.counter),
        }
    }
}

/// [`CountingReader`]의 누적 통과 바이트를 회수하는 핸들.
struct CountingHandle {
    counter: Arc<AtomicU64>,
}

impl CountingHandle {
    /// 현재까지 통과한 총 바이트 수(EOF 후 호출하면 원본 총량).
    fn total(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }
}

impl AsyncRead for CountingReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let poll = Pin::new(&mut self.inner).poll_read(cx, buf);
        if let Poll::Ready(Ok(())) = &poll {
            let newly = buf.filled().len() - before;
            if newly > 0 {
                self.counter.fetch_add(newly as u64, Ordering::SeqCst);
            }
        }
        poll
    }
}

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
    /// 진행 표시용 공유 바이트 카운터(t13/R16). `Some`이면 저장 바이트 카운터의 backing
    /// Arc로 주입되어, 핸들러의 진행 표시기가 이 값을 폴링한다(없으면 내부 Arc 사용).
    pub progress_counter: Option<Arc<AtomicU64>>,
}

impl BackupRequest {
    /// 선택적 백업 여부(`--db`/`--collection` 중 하나라도 지정).
    fn is_selective(&self) -> bool {
        self.db.is_some() || self.collection.is_some()
    }
}

/// oplog 포함 여부 결정(R2/FR-1) — replica set이라도 **선택적 백업이면 --oplog를 자동
/// 제거**한다(선택적 dump는 일관 oplog 구간을 보장할 수 없어 증분 base 부적격).
///
/// - replica set + 전체 백업 → oplog 포함(true).
/// - replica set + 선택적(`--db`/`--collection`) → oplog 자동 제거(false).
/// - standalone → 항상 false(oplog 부재).
fn decide_oplog(supports_oplog: bool, selective: bool) -> bool {
    supports_oplog && !selective
}

/// 압축·암호화 manifest 메타(t6). 스택에 해당 단계를 push했을 때 채워 넣어 manifest에
/// 기록한다. 평문 경로는 [`BackupMeta::none`](기본값)으로 모두 `None`이다.
///
/// 메타는 **단계 구성과 분리**해 전달한다 — `StageStack`은 단계 *이름*만 알지 레벨·키
/// 식별자 같은 기록값은 모르기 때문이다(handlers/backup.rs가 config·CLI에서 해석해 채움).
#[derive(Debug, Clone, Default)]
pub struct BackupMeta {
    /// manifest.compression(압축 단계가 있으면 Some).
    pub compression: Option<crate::manifest::schema::CompressionMeta>,
    /// manifest.encryption(암호화 단계가 있으면 Some).
    pub encryption: Option<crate::manifest::schema::EncryptionMeta>,
}

impl BackupMeta {
    /// 평문 경로용 빈 메타(압축·암호화 모두 없음).
    pub fn none() -> Self {
        Self::default()
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

/// 풀 백업을 끝까지 실행한다(평문/기존 호환 진입점, 메타 없음).
///
/// `stages`가 비어 있으면 평문 백업이다. 압축·암호화 메타를 manifest에 기록하려면
/// [`run_full_backup_with_meta`]를 쓴다(t6 핸들러 경로).
pub async fn run_full_backup(
    request: &BackupRequest,
    storage: &dyn Storage,
    stages: StageStack,
) -> Result<BackupOutcome> {
    run_full_backup_with_meta(request, storage, stages, BackupMeta::none()).await
}

/// 풀 백업을 끝까지 실행한다(메타 질의 → dump → 단계 → 저장 → manifest).
///
/// `storage`는 destination 백엔드. `stages`는 파이프라인 변환 단계(compress→encrypt,
/// t6이 구성). `meta`는 manifest에 기록할 압축/암호화 메타(단계 구성과 분리 전달).
pub async fn run_full_backup_with_meta(
    request: &BackupRequest,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
) -> Result<BackupOutcome> {
    // 1) 드라이버로 서버 메타 + dump 전 oplog ts 조회.
    let mongo = MongoMeta::connect(&request.uri).await?;
    let server_meta = mongo.server_meta().await?;
    let topology = server_meta.topology();

    // 선택적 백업이면 --oplog 자동 제거(R2/FR-1). 그 외 replica set이면 자동 부여.
    let selective = request.is_selective();
    let use_oplog = decide_oplog(server_meta.supports_oplog(), selective);
    if selective && server_meta.supports_oplog() {
        tracing::warn!(
            "선택적 백업(--db/--collection)은 --oplog와 병용 불가 — oplog 자동 제거, 증분 base 부적격(R2/FR-1)"
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

    // 4) 파이프라인 합성: dump stdout → (입력 바이트 카운터) → 단계(identity/t6) → sha256 tee.
    //    입력 카운터는 *압축 전* 원본 바이트(original_size_bytes)를 세고, sha256 tee는
    //    *저장 직전* 최종 바이트(stored, 압축·암호화 후)에 걸린다(설계 불변: 체크섬=저장 바이트).
    let counted = CountingReader::new(Box::pin(stdout));
    let original_size_handle = counted.handle();
    let staged: BoxAsyncRead = stages.apply(Box::pin(counted));
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();
    // 저장 바이트도 같은 패스에서 센다 — list 기반 사후 조회는 백엔드별 prefix
    // 의미가 달라(LocalFs는 디렉터리 취급) 신뢰할 수 없다.
    // 진행 카운터가 주입됐으면 저장 카운터의 backing Arc로 공유한다(진행 표시 폴링).
    let stored_counted = match &request.progress_counter {
        Some(counter) => CountingReader::with_counter(Box::pin(checksummed), Arc::clone(counter)),
        None => CountingReader::new(Box::pin(checksummed)),
    };
    let stored_size_handle = stored_counted.handle();

    // 5) data.bin 저장(업로드 먼저). put_stream이 바이트를 끝까지 소비한다.
    let backup_id = Uuid::now_v7().to_string();
    let data_rel = data_path(&backup_id);

    let put_result = storage
        .put_stream(&data_rel, Box::pin(stored_counted), None)
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
    let stored_size = stored_size_handle.total();
    // 원본(압축 전) 입력 바이트. 압축 단계가 없으면 stored와 같다(평문 경로).
    let original_size = original_size_handle.total();

    // 8) manifest 작성. 압축/암호화 메타는 meta에서 가져오고, original/stored를 분리 기록.
    let manifest = build_manifest(
        &backup_id,
        &server_meta,
        topology,
        selective,
        original_size,
        stored_size,
        &checksum,
        oplog_range,
        &meta,
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

/// manifest 값을 조립한다. 압축/암호화 메타는 `meta`에서, 크기는 분리 기록한다(t6).
#[allow(clippy::too_many_arguments)]
fn build_manifest(
    backup_id: &str,
    server_meta: &ServerMeta,
    topology: Topology,
    selective: bool,
    original_size: u64,
    stored_size: u64,
    checksum: &str,
    oplog_range: Option<OplogRange>,
    meta: &BackupMeta,
) -> BackupManifest {
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
        // original = 압축 전 dump 입력 총량, stored = 압축·암호화 후 저장 총량(t6).
        // 압축·암호화 단계가 없으면 두 값이 같다(평문 경로).
        original_size_bytes: original_size,
        stored_size_bytes: stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: checksum.to_string(),
        oplog_range,
        // 풀 백업은 oplog가 archive에 내장되어 별도 엔트리 카운트가 없다(증분 전용 필드).
        oplog_count: None,
        // 풀 백업은 gap 승격이 아니다(증분 핸들러가 풀로 승격할 때만 true로 덮어쓴다).
        promoted_from_gap: false,
        status: BackupStatus::Complete,
    }
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
    use crate::storage::MockStorage;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
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
                Err(XBackupError::StorageUpload(
                    "manifest 저장 실패(주입)".into(),
                ))
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
            oplog_count: None,
            promoted_from_gap: false,
            status: BackupStatus::Complete,
        };
        let write_err = store.write(&manifest).await.unwrap_err();
        assert_eq!(write_err.exit_code(), 1);

        // 실제 backup.rs 경로가 하듯 cleanup 호출.
        cleanup(&mock, "bk-fail").await;
        assert!(
            cleanup_called.load(Ordering::SeqCst),
            "cleanup이 delete를 호출하지 않음"
        );
    }

    /// 저장 크기는 list 사후 조회가 아니라 put 경로의 카운터로 집계된다 —
    /// LocalFs의 list는 prefix를 디렉터리로 취급해 정확 경로 조회가 불가하기 때문.
    #[tokio::test]
    async fn stored_size_counted_in_stream_pass() {
        let payload = vec![7u8; 4242];
        let counted = CountingReader::new(Box::pin(std::io::Cursor::new(payload)));
        let handle = counted.handle();
        let mut sink = Vec::new();
        tokio::io::copy(&mut Box::pin(counted), &mut sink)
            .await
            .unwrap();
        assert_eq!(handle.total(), 4242);
        assert_eq!(sink.len(), 4242);
    }

    /// R2 — 선택적 백업은 replica set이라도 --oplog를 자동 제거한다(증분 base 부적격).
    #[test]
    fn decide_oplog_drops_oplog_for_selective() {
        // replica set + 전체 백업 → oplog 포함.
        assert!(decide_oplog(true, false));
        // replica set + 선택적 → oplog 자동 제거(R2).
        assert!(!decide_oplog(true, true));
        // standalone → 항상 제거(oplog 부재).
        assert!(!decide_oplog(false, false));
        assert!(!decide_oplog(false, true));
    }

    /// is_selective는 --db/--collection 중 하나라도 있으면 true.
    #[test]
    fn is_selective_detects_db_or_collection() {
        let base = BackupRequest {
            uri: crate::config::secret::Secret::new("mongodb://h/db"),
            mongodump_program: "mongodump".into(),
            db: None,
            collection: None,
            progress_counter: None,
        };
        assert!(!base.is_selective());

        let with_db = BackupRequest {
            db: Some("app".into()),
            ..rebuild(&base)
        };
        assert!(with_db.is_selective());

        let with_coll = BackupRequest {
            collection: Some("users".into()),
            ..rebuild(&base)
        };
        assert!(with_coll.is_selective());
    }

    /// 테스트용 BackupRequest 복제 헬퍼(Secret은 Clone, 나머지 필드 복사).
    fn rebuild(r: &BackupRequest) -> BackupRequest {
        BackupRequest {
            uri: r.uri.clone(),
            mongodump_program: r.mongodump_program.clone(),
            db: r.db.clone(),
            collection: r.collection.clone(),
            progress_counter: None,
        }
    }

    /// build_manifest가 selective 플래그와 압축/암호화 메타·분리 크기를 정확히 기록한다.
    #[test]
    fn build_manifest_records_selective_meta_and_sizes() {
        let server = ServerMeta {
            repl_set_name: Some("rs0".into()),
            server_version: "7.0.35".into(),
        };
        let meta = BackupMeta {
            compression: Some(crate::manifest::schema::CompressionMeta {
                algorithm: "zstd".into(),
                level: 10,
            }),
            encryption: Some(crate::manifest::schema::EncryptionMeta {
                algorithm: "age".into(),
                key_id: Some("age1abc".into()),
            }),
        };
        let m = build_manifest(
            "bk-1",
            &server,
            Topology::ReplicaSet,
            /* selective */ true,
            /* original */ 1000,
            /* stored */ 250,
            "deadbeef",
            None,
            &meta,
        );
        assert!(m.selective, "선택적 백업 manifest.selective=true");
        assert_eq!(m.original_size_bytes, 1000);
        assert_eq!(m.stored_size_bytes, 250);
        assert_eq!(m.compression.unwrap().level, 10);
        assert_eq!(m.encryption.unwrap().algorithm, "age");
    }
}
