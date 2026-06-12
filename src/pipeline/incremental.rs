//! 증분 백업 파이프라인 — base 선택 → gap 감지 → oplog 캡처 → 저장 → manifest(R3/R4).
//!
//! 풀 백업([`super::backup`])이 `mongodump --archive`로 시점 일관성 스냅샷을 담는 것과
//! 달리, 증분은 드라이버로 `local.oplog.rs`를 직접 질의해 `last_backup_ts` 이후의 oplog
//! 슬라이스를 캡처한다(PRD §6.3, FR-2). 캡처 스트림은 풀 백업과 **동일한 StageStack**
//! (압축·암호화)·sha256 tee·put_stream 경로를 그대로 통과한다.
//!
//! ## 흐름
//! ```text
//! list+manifest로 base(체인 끝 Complete) 선택 → last_backup_ts = base.oplog_range.end_ts
//!   → [standalone? → 거부(exit 1)]
//!   → gap 감지(캡처 전): last_backup_ts가 oplog 윈도우 안에 존재하는가?
//!       ├─ gap(롤오버)      → 풀 백업 승격(promoted_from_gap=true) + exit 4 (SC2)
//!       └─ ok              → 상한 ts 고정(partialTxn 경계 확장)
//!            → capture_stream(raw BSON) → StageStack → sha256 → put_stream
//!            → [late gap(CursorNotFound) → 부분 산출물 정리 + 풀 승격 + exit 4]
//!            → [빈 슬라이스(0건) → data 업로드 없이 manifest만(oplog_count=0)]
//!            → manifest(Incremental, base_id, oplog_range, oplog_count)
//! ```
//!
//! ## 무결성 순서(pitfall 7-1)
//! 풀 백업과 동일하게 "data 먼저, manifest 나중"이며, 실패·late gap 경로에서 부분
//! 산출물(data.bin)을 best-effort 정리한다. 빈 슬라이스는 data가 없으므로 manifest만
//! 기록한다(`stored_size_bytes=0`, `oplog_count=0` — verify/t10와의 계약은 schema 주석).

use chrono::Utc;
use uuid::Uuid;

use crate::engine::mongo::oplog::CaptureError;
use crate::engine::mongo::{GapCheck, MongoMeta, OplogReader};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{
    BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology, ToolVersions,
    FORMAT_VERSION,
};
use crate::manifest::store::{data_path, manifest_path, manifest_sha_path, ManifestStore};
use crate::pipeline::backup::{
    run_full_backup_with_meta, BackupMeta, BackupOutcome, BackupRequest,
};
use crate::pipeline::checksum::Sha256Reader;
use crate::pipeline::stage::StageStack;
use crate::storage::{BoxAsyncRead, Storage};

/// 증분 백업 요청.
pub struct IncrementalRequest {
    /// 연결·oplog 질의에 쓸 MongoDB URI 시크릿.
    pub uri: crate::config::secret::Secret,
    /// 풀 승격 시 쓸 mongodump 실행파일 경로(보통 `"mongodump"`).
    pub mongodump_program: String,
}

/// 증분 백업 결과 — 일반 증분이거나 gap으로 승격된 풀 백업.
#[derive(Debug, Clone)]
pub enum IncrementalOutcome {
    /// 증분 백업이 정상 산출됨.
    Captured {
        /// 백업 ID.
        backup_id: String,
        /// 연결된 base 백업 ID.
        base_id: String,
        /// 캡처한 oplog 엔트리 개수(0이면 빈 슬라이스).
        oplog_count: u64,
        /// 캡처 구간(last_backup_ts → 마지막 캡처 ts).
        oplog_range: OplogRange,
        /// 저장 바이트(빈 슬라이스면 0, data.bin 미생성).
        stored_size_bytes: u64,
    },
    /// gap(또는 late gap) 감지로 풀 백업으로 승격됨 — 호출자는 **exit 4**로 보고한다(SC2).
    PromotedToFull {
        /// 승격으로 생성된 풀 백업 결과.
        outcome: BackupOutcome,
        /// 승격 사유(로그·보고용).
        reason: String,
    },
}

/// 증분 base 선택 결과 — 체인 끝의 Complete 백업과 그 기준점 ts.
#[derive(Debug, Clone)]
struct BaseSelection {
    /// base 백업 ID.
    id: String,
    /// 다음 증분의 시작 기준점 = base manifest의 oplog_range.end_ts.
    last_backup_ts: OplogTimestamp,
}

/// 증분 백업을 끝까지 실행한다(base 선택 → gap → 캡처/승격 → manifest).
///
/// `storage`는 destination 백엔드. `stage_factory`는 호출 시마다 **새** 파이프라인 단계
/// 스택·manifest 메타를 만드는 팩토리다(압축·암호화는 핸들러가 구성). 팩토리로 받는
/// 이유: [`StageStack`]은 `Box<dyn PipelineStage>`를 담아 `Clone`이 불가한데, 증분 캡처가
/// 스택을 소비한 *뒤에도* (late gap 등) 풀 승격을 위해 동일 구성의 스택을 새로 만들 수
/// 있어야 하기 때문이다. 캡처용 1회 + 승격이 필요하면 1회 더 호출된다.
///
/// 반환이 [`IncrementalOutcome::PromotedToFull`]이면 호출자는 exit 4(경고 동반 성공)로
/// 보고해야 한다.
pub async fn run_incremental_backup<F>(
    request: &IncrementalRequest,
    storage: &dyn Storage,
    stage_factory: F,
) -> Result<IncrementalOutcome>
where
    F: Fn() -> Result<(StageStack, BackupMeta)>,
{
    // 1) 토폴로지 확인 — standalone(oplog 부재)이면 사유와 함께 거부(exit 1, FR-2).
    let mongo = MongoMeta::connect(&request.uri).await?;
    let server_meta = mongo.server_meta().await?;
    if !server_meta.supports_oplog() {
        return Err(XBackupError::Failure(
            "standalone 서버는 oplog가 없어 증분 백업을 할 수 없습니다 — \
             replica set이 필요합니다(FR-2). 풀 백업(--type full)을 사용하세요."
                .into(),
        ));
    }

    // 2) base 선택 — 체인 끝의 Complete 백업(풀 또는 증분). 없거나 부적격이면 거부.
    let base = select_base(storage).await?;
    let last_backup_ts: bson::Timestamp = base.last_backup_ts.into();
    tracing::info!(
        base_id = %base.id,
        last_ts = ?base.last_backup_ts,
        "증분 base 선택 — 이 기준점 이후 oplog를 캡처합니다"
    );

    // 3) gap 감지(캡처 전) — 직전 기준점이 oplog 윈도우 안에 아직 있는가?(§6.2)
    let reader = OplogReader::connect(&request.uri).await?;
    match reader.detect_gap(last_backup_ts).await? {
        GapCheck::Ok { boundary_risk } => {
            if boundary_risk {
                tracing::warn!(
                    "last_backup_ts가 oplog 최소 ts와 같습니다($gte 경계 위험) — \
                     다음 폴 전 롤오버 가능. 증분 주기를 줄이세요(§3-1)."
                );
            }
        }
        GapCheck::OplogEmpty => {
            // oplog가 비었으면(드뭄) 캡처할 게 없고 기준점 존재 확인 불가 — 안전하게 승격.
            return promote_to_full(
                request,
                storage,
                &stage_factory,
                "oplog가 비어 있어 기준점 존재를 확인할 수 없음".into(),
            )
            .await;
        }
        GapCheck::Gap { last, min } => {
            let reason = format!(
                "oplog 윈도우 롤오버(gap) — 직전 기준점 {{t:{},i:{}}}이(가) 현재 oplog 최소 \
                 {{t:{},i:{}}}보다 과거입니다. 증분 체인이 끊겨 풀 백업으로 승격합니다(§6.2).",
                last.t, last.i, min.t, min.i
            );
            tracing::warn!("{reason}");
            return promote_to_full(request, storage, &stage_factory, reason).await;
        }
    }

    // 4) 상한 ts 고정 + partialTxn 경계 확장(캡처 도중 경계 이동 방지, 트랜잭션 온전 보장).
    let upper = match reader.resolve_upper_bound().await? {
        Some(ts) => ts,
        // 윈도우 내 기준점은 있는데 최신 ts가 없다(이론상 비정상) — 빈 슬라이스로 처리.
        None => last_backup_ts,
    };

    // 상한이 기준점 이하면 새 엔트리가 없다(빈 슬라이스). 캡처 없이 manifest만 기록.
    let upper_ord: OplogTimestamp = upper.into();
    if upper_ord <= base.last_backup_ts {
        return record_empty_slice(storage, &server_meta, &base).await;
    }

    // 5) 캡처 — raw BSON 스트림을 StageStack → sha256 → put_stream으로 흘린다(풀과 동일 경로).
    //    캡처용 스택을 팩토리로 1회 만든다(승격이 필요하면 팩토리를 다시 호출).
    let (stages, meta) = stage_factory()?;
    let capture = reader.capture_stream(last_backup_ts, upper);
    let capture_handle = capture.handle();

    let staged: BoxAsyncRead = stages.apply(Box::pin(capture));
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();
    let stored_counted = CountingReader::new(Box::pin(checksummed));
    let stored_size_handle = stored_counted.handle();

    let backup_id = Uuid::now_v7().to_string();
    let data_rel = data_path(&backup_id);

    // data.bin 업로드(업로드 먼저). put_stream이 캡처 스트림을 끝까지 소비한다.
    let put_result = storage
        .put_stream(&data_rel, Box::pin(stored_counted), None)
        .await;

    // 캡처 결과(엔트리 수·late gap) 회수 — 스트림 EOF 이후.
    let capture_result = capture_handle.finish().await;

    if let Err(put_err) = put_result {
        cleanup(storage, &backup_id).await;
        return Err(put_err);
    }

    // late gap이면 부분 산출물을 버리고 풀 승격(§6.2, pitfall 2-4).
    let oplog_count = match capture_result {
        Ok(count) => count,
        Err(CaptureError::LateGap(msg)) => {
            cleanup(storage, &backup_id).await;
            let reason = format!("캡처 중 late gap — {msg}");
            tracing::warn!("{reason} → 풀 백업으로 승격합니다.");
            return promote_to_full(request, storage, &stage_factory, reason).await;
        }
        Err(CaptureError::Other(e)) => {
            cleanup(storage, &backup_id).await;
            return Err(e);
        }
    };

    // 캡처는 성공했으나 엔트리 0건이면(상한==기준점 경계 등) 빈 슬라이스로 전환 —
    // data.bin이 빈 파일로 남지 않게 정리하고 manifest만 기록한다.
    if oplog_count == 0 {
        cleanup(storage, &backup_id).await;
        return record_empty_slice(storage, &server_meta, &base).await;
    }

    // 6) 체크섬·크기 확정 + manifest 작성(Incremental, base_id, oplog_range, oplog_count).
    let checksum = checksum_handle
        .finalize()
        .ok_or_else(|| XBackupError::Failure("체크섬 확정 실패(이미 소비됨)".into()))?;
    let stored_size = stored_size_handle.total();

    let oplog_range = OplogRange {
        start_ts: base.last_backup_ts,
        end_ts: upper_ord,
    };
    let manifest = build_incremental_manifest(
        &backup_id,
        &base.id,
        &server_meta,
        stored_size,
        &checksum,
        oplog_range,
        oplog_count,
        &meta,
    );

    let store = ManifestStore::new(storage);
    if let Err(write_err) = store.write(&manifest).await {
        cleanup(storage, &backup_id).await;
        return Err(write_err);
    }

    tracing::info!(
        backup_id = %backup_id,
        base_id = %base.id,
        entries = oplog_count,
        bytes = stored_size,
        "증분 백업 완료"
    );

    Ok(IncrementalOutcome::Captured {
        backup_id,
        base_id: base.id,
        oplog_count,
        oplog_range,
        stored_size_bytes: stored_size,
    })
}

/// 빈 슬라이스(엔트리 0건)를 manifest만으로 기록한다(pitfall 3-2).
///
/// data.bin을 업로드하지 않고(`stored_size_bytes=0`, `oplog_count=0`) manifest·사이드카만
/// 기록한다. `oplog_range`는 `start==end==last_backup_ts`다 — 다음 증분은 동일 기준점을
/// 그대로 이어받는다. verify/list는 `oplog_count==Some(0)`이면 data 부재를 정상으로 본다
/// (계약은 schema 주석에 명시).
async fn record_empty_slice(
    storage: &dyn Storage,
    server_meta: &crate::engine::mongo::ServerMeta,
    base: &BaseSelection,
) -> Result<IncrementalOutcome> {
    let backup_id = Uuid::now_v7().to_string();
    let checksum = empty_sha256();
    let oplog_range = OplogRange {
        start_ts: base.last_backup_ts,
        end_ts: base.last_backup_ts,
    };

    let manifest = build_incremental_manifest(
        &backup_id,
        &base.id,
        server_meta,
        /* stored_size */ 0,
        &checksum,
        oplog_range,
        /* oplog_count */ 0,
        &BackupMeta::none(),
    );

    let store = ManifestStore::new(storage);
    if let Err(write_err) = store.write(&manifest).await {
        cleanup(storage, &backup_id).await;
        return Err(write_err);
    }

    tracing::info!(
        backup_id = %backup_id,
        base_id = %base.id,
        "증분 백업 — 빈 슬라이스(변경 없음): data 없이 manifest만 기록(oplog_count=0)"
    );

    Ok(IncrementalOutcome::Captured {
        backup_id,
        base_id: base.id.clone(),
        oplog_count: 0,
        oplog_range,
        stored_size_bytes: 0,
    })
}

/// gap·late gap·oplog-empty 시 풀 백업으로 승격한다(promoted_from_gap=true, exit 4 경로).
///
/// 승격은 `stage_factory`로 새 스택을 만들어 [`run_full_backup_with_meta`]를 호출한 뒤,
/// 산출된 manifest를 다시 읽어 `promoted_from_gap` 표식을 세워 덮어쓴다(additive 필드).
/// 풀 백업의 무결성 순서·정리 규약을 그대로 재사용하므로 별도 위험이 없다.
async fn promote_to_full<F>(
    request: &IncrementalRequest,
    storage: &dyn Storage,
    stage_factory: &F,
    reason: String,
) -> Result<IncrementalOutcome>
where
    F: Fn() -> Result<(StageStack, BackupMeta)>,
{
    let (stages, meta) = stage_factory()?;
    let full_request = BackupRequest {
        uri: request.uri.clone(),
        mongodump_program: request.mongodump_program.clone(),
        db: None,
        collection: None,
    };
    let outcome = run_full_backup_with_meta(&full_request, storage, stages, meta).await?;

    // 승격 표식 추가 — manifest를 다시 읽어 promoted_from_gap=true로 덮어쓴다.
    let store = ManifestStore::new(storage);
    match store.read(&outcome.backup_id).await {
        Ok(mut manifest) => {
            manifest.promoted_from_gap = true;
            if let Err(e) = store.write(&manifest).await {
                // 표식 기록 실패는 백업 자체를 무효화하지 않는다(데이터는 정상) — 경고만.
                tracing::warn!(
                    backup_id = %outcome.backup_id,
                    "promoted_from_gap 표식 기록 실패(백업은 유효): {e}"
                );
            }
        }
        Err(e) => {
            tracing::warn!(
                backup_id = %outcome.backup_id,
                "승격 manifest 재조회 실패(백업은 유효): {e}"
            );
        }
    }

    Ok(IncrementalOutcome::PromotedToFull { outcome, reason })
}

/// 증분 base를 선택한다 — 저장된 manifest 중 체인 끝의 **Complete & 증분 적격** 백업.
///
/// 적격 규칙(태스크 지침 1):
/// - `status == Complete`(부분 성공 incomplete는 base 부적격).
/// - `selective == false`(선택적 백업은 일관 oplog 구간 보장 불가 → incr_base 부적격).
/// - `oplog_range`가 있어야 한다(end_ts가 다음 기준점). standalone 풀백업은 없으므로 제외.
/// - 풀/증분 모두 가능 — 체인 끝(가장 최신 적격 백업)을 고른다.
///
/// 최신성은 manifest.id(UUID v7, 시간 정렬 가능 = 사전순)로 판단한다. 적격 base가
/// 없으면 사유와 함께 거부한다(exit 2 — 사용법/전제 미충족).
async fn select_base(storage: &dyn Storage) -> Result<BaseSelection> {
    let store = ManifestStore::new(storage);

    // 저장된 모든 `<id>/manifest.json`을 열거한다.
    let entries = storage.list("").await?;
    let mut manifest_ids: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            e.path
                .strip_suffix("/manifest.json")
                .map(|id| id.to_string())
        })
        .collect();
    // UUID v7 사전순 = 생성순. 최신부터 보도록 내림차순.
    manifest_ids.sort();
    manifest_ids.reverse();

    let mut saw_incomplete = false;
    let mut saw_selective = false;
    for id in &manifest_ids {
        let manifest = match store.read(id).await {
            Ok(m) => m,
            // 깨진 manifest는 건너뛴다(verify가 별도 보고; base 선택은 멈추지 않음).
            Err(e) => {
                tracing::debug!(backup_id = %id, "base 후보 manifest 읽기 실패(건너뜀): {e}");
                continue;
            }
        };
        if !matches!(manifest.status, BackupStatus::Complete) {
            saw_incomplete = true;
            continue;
        }
        if manifest.selective {
            saw_selective = true;
            continue;
        }
        let end_ts = match &manifest.oplog_range {
            Some(range) => range.end_ts,
            // oplog_range가 없으면(예: standalone 풀백업) 증분 base 부적격.
            None => continue,
        };
        return Ok(BaseSelection {
            id: manifest.id,
            last_backup_ts: end_ts,
        });
    }

    // 적격 base가 없음 — 가장 그럴듯한 사유를 골라 거부한다(exit 2).
    let detail = if manifest_ids.is_empty() {
        "저장소에 백업이 없습니다 — 먼저 풀 백업(--type full)을 한 번 수행하세요."
    } else if saw_selective {
        "적격 base가 없습니다 — 선택적 백업(--db/--collection)은 증분 base로 쓸 수 없습니다(FR-1)."
    } else if saw_incomplete {
        "적격 base가 없습니다 — 가장 최근 백업이 incomplete(부분 성공)라 base로 쓸 수 없습니다."
    } else {
        "적격 base가 없습니다 — oplog 구간이 기록된 Complete 풀/증분 백업이 필요합니다."
    };
    Err(XBackupError::Usage(detail.into()))
}

/// 증분 manifest 값을 조립한다(backup_type=Incremental, base_id·oplog_range·oplog_count).
#[allow(clippy::too_many_arguments)]
fn build_incremental_manifest(
    backup_id: &str,
    base_id: &str,
    server_meta: &crate::engine::mongo::ServerMeta,
    stored_size: u64,
    checksum: &str,
    oplog_range: OplogRange,
    oplog_count: u64,
    meta: &BackupMeta,
) -> BackupManifest {
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: backup_id.to_string(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Incremental,
        base_id: Some(base_id.to_string()),
        topology: Topology::ReplicaSet,
        server_version: server_meta.server_version.clone(),
        tool_versions: ToolVersions::default(),
        // 증분은 항상 전체 oplog 슬라이스(선택적 아님).
        selective: false,
        // 증분은 oplog 슬라이스의 원본/저장 바이트가 같은 파이프라인을 타지만, original은
        // 압축 전 입력으로 별도 집계하지 않고 stored와 동일하게 둔다(빈 슬라이스는 0).
        original_size_bytes: stored_size,
        stored_size_bytes: stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: checksum.to_string(),
        oplog_range: Some(oplog_range),
        oplog_count: Some(oplog_count),
        promoted_from_gap: false,
        status: BackupStatus::Complete,
    }
}

/// 빈 입력의 sha256(빈 슬라이스 data 부재 시 manifest checksum 자리값).
fn empty_sha256() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(b""))
}

/// 부분 산출물 정리 — 실패/late-gap 경로에서 백업 디렉터리의 알려진 파일을 best-effort
/// 삭제한다(backup.rs::cleanup과 동형: data → manifest → 사이드카).
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

// ── 압축 전 통과 바이트 카운터(backup.rs와 동형 — 저장 바이트 집계용) ──
// backup.rs의 CountingReader는 모듈 private이라 재사용 불가하므로 동일 구현을 둔다.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, ReadBuf};

/// 통과 바이트 수를 세는 [`AsyncRead`] 래퍼(저장 바이트 집계).
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
    fn handle(&self) -> CountingHandle {
        CountingHandle {
            counter: Arc::clone(&self.counter),
        }
    }
}

struct CountingHandle {
    counter: Arc<AtomicU64>,
}

impl CountingHandle {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{CompressionMeta, EncryptionMeta};
    use crate::storage::LocalFs;

    fn server() -> crate::engine::mongo::ServerMeta {
        crate::engine::mongo::ServerMeta {
            repl_set_name: Some("rs0".into()),
            server_version: "7.0.35".into(),
        }
    }

    /// 풀백업 manifest를 storage에 직접 써 base 후보를 만든다(테스트 픽스처).
    async fn write_full(
        storage: &dyn Storage,
        id: &str,
        status: BackupStatus,
        selective: bool,
        oplog_range: Option<OplogRange>,
    ) {
        let m = BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.into(),
            created_at: "2026-06-12T00:00:00Z".into(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: ToolVersions::default(),
            selective,
            original_size_bytes: 10,
            stored_size_bytes: 10,
            compression: None,
            encryption: None,
            checksum_sha256: "x".into(),
            oplog_range,
            oplog_count: None,
            promoted_from_gap: false,
            status,
        };
        ManifestStore::new(storage).write(&m).await.unwrap();
    }

    fn range(t: u32, i: u32) -> OplogRange {
        OplogRange {
            start_ts: OplogTimestamp::new(t, i),
            end_ts: OplogTimestamp::new(t, i),
        }
    }

    /// base 선택: Complete·비-selective·oplog_range 있는 최신 백업을 고른다.
    #[tokio::test]
    async fn select_base_picks_latest_eligible() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // UUID v7는 사전순=생성순. 사전순으로 b > a 가 되도록 id를 둔다.
        write_full(&fs, "00000000-aaaa", BackupStatus::Complete, false, Some(range(100, 1))).await;
        write_full(&fs, "ffffffff-bbbb", BackupStatus::Complete, false, Some(range(200, 2))).await;

        let base = select_base(&fs).await.unwrap();
        assert_eq!(base.id, "ffffffff-bbbb");
        assert_eq!(base.last_backup_ts, OplogTimestamp::new(200, 2));
    }

    /// base 선택: incomplete·selective·oplog_range 없는 후보는 건너뛴다.
    #[tokio::test]
    async fn select_base_skips_ineligible_candidates() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        // 최신순으로: incomplete(스킵) → selective(스킵) → no-range(스킵) → 적격.
        write_full(&fs, "aa-eligible", BackupStatus::Complete, false, Some(range(50, 1))).await;
        write_full(&fs, "bb-norange", BackupStatus::Complete, false, None).await;
        write_full(&fs, "cc-selective", BackupStatus::Complete, true, Some(range(80, 1))).await;
        write_full(&fs, "dd-incomplete", BackupStatus::Incomplete, false, Some(range(90, 1))).await;

        let base = select_base(&fs).await.unwrap();
        assert_eq!(base.id, "aa-eligible");
    }

    /// base 선택: 백업이 하나도 없으면 Usage 에러(exit 2).
    #[tokio::test]
    async fn select_base_errors_when_empty() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let err = match select_base(&fs).await {
            Ok(_) => panic!("빈 저장소인데 base 선택 성공"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), 2);
    }

    /// base 선택: selective만 있으면 사유에 선택적 백업을 명시하고 거부한다.
    #[tokio::test]
    async fn select_base_rejects_only_selective() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        write_full(&fs, "aa-selective", BackupStatus::Complete, true, Some(range(50, 1))).await;
        let err = match select_base(&fs).await {
            Ok(_) => panic!("selective base는 거부해야 함"),
            Err(e) => e,
        };
        assert_eq!(err.exit_code(), 2);
        assert!(err.to_string().contains("선택적"), "사유 메시지: {err}");
    }

    /// 빈 슬라이스 manifest: oplog_count=0, stored=0, start==end==last, data 없음.
    #[tokio::test]
    async fn empty_slice_records_manifest_only() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let base = BaseSelection {
            id: "base-1".into(),
            last_backup_ts: OplogTimestamp::new(123, 4),
        };

        let outcome = record_empty_slice(&fs, &server(), &base).await.unwrap();
        let (backup_id, count, stored) = match outcome {
            IncrementalOutcome::Captured {
                backup_id,
                oplog_count,
                stored_size_bytes,
                oplog_range,
                base_id,
            } => {
                assert_eq!(base_id, "base-1");
                assert_eq!(oplog_range.start_ts, oplog_range.end_ts);
                assert_eq!(oplog_range.end_ts, OplogTimestamp::new(123, 4));
                (backup_id, oplog_count, stored_size_bytes)
            }
            other => panic!("Captured를 기대했으나: {other:?}"),
        };
        assert_eq!(count, 0);
        assert_eq!(stored, 0);

        // manifest는 있고 data.bin은 없어야 한다(빈 슬라이스 계약).
        let base_dir = dir.path().join(&backup_id);
        assert!(base_dir.join("manifest.json").exists(), "manifest 없음");
        assert!(!base_dir.join("data.bin").exists(), "빈 슬라이스인데 data.bin 존재");

        // manifest 내용 검증.
        let m = ManifestStore::new(&fs).read(&backup_id).await.unwrap();
        assert_eq!(m.backup_type, BackupType::Incremental);
        assert_eq!(m.base_id.as_deref(), Some("base-1"));
        assert_eq!(m.oplog_count, Some(0));
        assert_eq!(m.stored_size_bytes, 0);
        assert!(!m.promoted_from_gap);
    }

    /// build_incremental_manifest가 base_id·ts범위·count·압축/암호화 메타를 기록한다.
    #[test]
    fn build_incremental_manifest_records_chain_fields() {
        let meta = BackupMeta {
            compression: Some(CompressionMeta {
                algorithm: "zstd".into(),
                level: 10,
            }),
            encryption: Some(EncryptionMeta {
                algorithm: "age".into(),
                key_id: Some("age1abc".into()),
            }),
        };
        let r = OplogRange {
            start_ts: OplogTimestamp::new(100, 1),
            end_ts: OplogTimestamp::new(150, 3),
        };
        let m = build_incremental_manifest(
            "incr-1", "base-1", &server(), 4242, "deadbeef", r, 17, &meta,
        );
        assert_eq!(m.backup_type, BackupType::Incremental);
        assert_eq!(m.base_id.as_deref(), Some("base-1"));
        assert_eq!(m.oplog_count, Some(17));
        assert_eq!(m.oplog_range.unwrap().end_ts, OplogTimestamp::new(150, 3));
        assert_eq!(m.stored_size_bytes, 4242);
        assert_eq!(m.compression.unwrap().level, 10);
        assert_eq!(m.encryption.unwrap().algorithm, "age");
        assert!(!m.selective, "증분은 selective=false");
    }
}
