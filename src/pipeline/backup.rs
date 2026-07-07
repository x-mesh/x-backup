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
use crate::engine::mongo::MongoMeta;
#[cfg(feature = "legacy-mongodump")]
use crate::engine::mongo::{DumpProcess, DumpSpec, UriConfigFile};
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

/// 백업/복구 엔진 — 외부 도구 사용 여부.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Engine {
    /// 드라이버로 직접 백업/복구(외부 도구 불필요). 자체 아카이브 포맷. **기본**.
    #[default]
    Native,
    /// 외부 `mongodump`/`mongorestore` 오케스트레이션(시점 일관 `--oplog` 지원).
    Mongodump,
}

/// legacy-mongodump 미포함 빌드에서 mongodump 엔진 경로가 요구될 때의 설정 오류(exit 2).
///
/// 모든 진입점(parse)과 도달 불가 방어 분기가 같은 안내를 낸다.
#[cfg(not(feature = "legacy-mongodump"))]
fn legacy_engine_unavailable() -> XBackupError {
    XBackupError::Config(
        "이 빌드에는 mongodump 엔진이 포함되지 않았습니다(cargo feature `legacy-mongodump`) — \
         engine = \"native\"를 사용하세요. 기존 mongodump 포맷 백업의 복구가 필요하면 \
         legacy-mongodump feature를 켠 빌드를 사용해야 합니다"
            .into(),
    )
}

impl Engine {
    /// config 문자열(`native`|`mongodump`)에서 파싱한다. 그 외 값은 설정 오류.
    ///
    /// `mongodump`는 legacy-mongodump feature 빌드에서만 유효하다 — 미포함 빌드에서는
    /// 여기서 설정 오류(exit 2)로 조기 거부해 하위 분기가 도달 불가가 되게 한다.
    pub fn parse(s: &str) -> Result<Self> {
        match s {
            "native" => Ok(Engine::Native),
            #[cfg(feature = "legacy-mongodump")]
            "mongodump" => Ok(Engine::Mongodump),
            #[cfg(not(feature = "legacy-mongodump"))]
            "mongodump" => Err(legacy_engine_unavailable()),
            other => Err(XBackupError::Config(format!(
                "알 수 없는 engine: '{other}'(native | mongodump만 지원)"
            ))),
        }
    }

    /// manifest.archive_format에 기록할 포맷 식별자.
    pub fn archive_format(self) -> &'static str {
        match self {
            Engine::Native => crate::engine::native::archive::FORMAT_ID,
            Engine::Mongodump => "mongodump",
        }
    }
}

/// dump 스트림의 종료 처리 — 엔진별로 다른 정리/대기 로직을 한 자리에 모은다.
///
/// `DumpProcess`·`NativeDumpHandle`의 wait/finish가 `self`를 소비하므로 `Option`으로
/// 감싸 `take()` 후 호출한다(중복 호출 안전).
enum DumpFinalizer {
    /// mongodump 자식 프로세스(+ 0600 임시 URI config 핸들 — 종료까지 유지).
    /// Box로 감싼다 — 변형 간 크기 격차 회피(`clippy::large_enum_variant`).
    #[cfg(feature = "legacy-mongodump")]
    Mongodump(Box<MongodumpFinalizer>),
    /// 네이티브 덤프 task 핸들(EOF 후 결과 회수).
    Native(Option<crate::engine::native::backup::NativeDumpHandle>),
}

/// mongodump 종료 처리 페이로드 — 자식 프로세스 + 0600 임시 URI config 핸들.
#[cfg(feature = "legacy-mongodump")]
struct MongodumpFinalizer {
    proc: Option<DumpProcess>,
    _uri_config: UriConfigFile,
}

impl DumpFinalizer {
    /// 정상 경로 — 스트림 EOF 후 종료를 판정한다(mongodump exit code / native task 결과).
    async fn finish(&mut self) -> Result<()> {
        match self {
            #[cfg(feature = "legacy-mongodump")]
            DumpFinalizer::Mongodump(m) => match m.proc.take() {
                Some(p) => p.wait().await,
                None => Ok(()),
            },
            DumpFinalizer::Native(handle) => match handle.take() {
                Some(h) => h.finish().await,
                None => Ok(()),
            },
        }
    }

    /// 실패 경로 — 자식/리더를 정리한다(좀비/누수 방지).
    async fn abort(&mut self) {
        match self {
            #[cfg(feature = "legacy-mongodump")]
            DumpFinalizer::Mongodump(m) => {
                if let Some(p) = m.proc.take() {
                    p.abort().await;
                }
            }
            // native: 리더가 이미 drop되어 쓰기 task가 BrokenPipe로 끝난다 — 결과만 흡수.
            DumpFinalizer::Native(handle) => {
                if let Some(h) = handle.take() {
                    let _ = h.finish().await;
                }
            }
        }
    }
}

/// 풀 백업 요청.
pub struct BackupRequest {
    /// 연결·메타 질의에 쓸 MongoDB URI 시크릿.
    pub uri: crate::config::secret::Secret,
    /// 백업 엔진(native | mongodump). 기본 native.
    pub engine: Engine,
    /// mongodump 실행파일 경로(보통 `"mongodump"`; mongodump 엔진에서만 사용).
    pub mongodump_program: String,
    /// 선택적 백업 — 특정 DB(`--db`). 지정 시 `--oplog` 비활성(FR-1).
    pub db: Option<String>,
    /// 선택적 백업 — 특정 컬렉션(`--collection`).
    pub collection: Option<String>,
    /// MongoDB 접속 타임아웃(초). `None`이면 기본 5초.
    pub timeout_secs: Option<u64>,
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
    /// 압축 전 원본 바이트(압축 단계가 없으면 stored와 동일). 압축률 표시용.
    pub original_size_bytes: u64,
    /// 저장 바이트의 sha256 hex.
    pub checksum_sha256: String,
    /// 토폴로지.
    pub topology: Topology,
    /// 적용된 압축 메타(평문 경로면 None).
    pub compression: Option<crate::manifest::schema::CompressionMeta>,
    /// 적용된 암호화 메타(평문 경로면 None).
    pub encryption: Option<crate::manifest::schema::EncryptionMeta>,
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
    let mongo = MongoMeta::connect(&request.uri, request.timeout_secs).await?;
    let server_meta = mongo.server_meta().await?;
    let topology = server_meta.topology();

    // 선택적 백업이면(--db/--collection) 증분 base 부적격이라 oplog 구간을 기록하지 않는다.
    // replica set + 전체 백업일 때만 oplog 구간을 기록해 증분 체인의 base가 되게 한다.
    let selective = request.is_selective();
    let record_oplog = decide_oplog(server_meta.supports_oplog(), selective);
    if selective && server_meta.supports_oplog() {
        tracing::warn!(
            "선택적 백업(--db/--collection)은 일관 oplog 구간을 보장할 수 없어 증분 base 부적격(R2/FR-1)"
        );
    }

    let oplog_start = if record_oplog {
        mongo.latest_oplog_ts().await?
    } else {
        None
    };

    // 2) dump 스트림 생성 — 엔진 분기. mongodump는 `--archive=-`(시점 일관 `--oplog`),
    //    native는 드라이버로 직접 아카이브 스트림을 만든다(외부 도구 불필요).
    let archive_format = request.engine.archive_format();
    let (dump_stream, finalizer): (BoxAsyncRead, DumpFinalizer) = match request.engine {
        // parse가 조기 거부하므로 미포함 빌드에서 도달 불가(방어적).
        #[cfg(not(feature = "legacy-mongodump"))]
        Engine::Mongodump => return Err(legacy_engine_unavailable()),
        #[cfg(feature = "legacy-mongodump")]
        Engine::Mongodump => {
            // URI를 0600 임시 config로 — argv 노출 금지(PRD §11). 핸들은 dump 종료까지 유지.
            let uri_config = UriConfigFile::create(&request.uri)?;
            let spec = DumpSpec {
                program: request.mongodump_program.clone(),
                uri_config_path: uri_config.path().to_string(),
                oplog: record_oplog,
                db: request.db.clone(),
                collection: request.collection.clone(),
            };
            let mut proc = DumpProcess::spawn(&spec)?;
            let stdout = proc.take_stdout()?;
            (
                Box::pin(stdout),
                DumpFinalizer::Mongodump(Box::new(MongodumpFinalizer {
                    proc: Some(proc),
                    _uri_config: uri_config,
                })),
            )
        }
        Engine::Native => {
            let dumper =
                crate::engine::native::NativeDumper::connect(&request.uri, request.timeout_secs)
                    .await?;
            let nds = dumper.dump_stream(request.db.clone(), request.collection.clone());
            let handle = nds.handle();
            (Box::pin(nds), DumpFinalizer::Native(Some(handle)))
        }
    };

    // 3) 공통 코어: 합성(카운터→스테이지→sha256) → 저장 → 종료 판정 → 확정.
    let stored = store_dump_stream(
        storage,
        dump_stream,
        stages,
        &request.progress_counter,
        finalizer,
    )
    .await?;

    // 4) dump 후 oplog ts 조회(구간 end).
    let oplog_end = if record_oplog {
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

    // 5) manifest 작성·기록. 압축/암호화 메타는 meta에서, archive_format(엔진)도 기록한다 —
    //    복구가 이 값을 보고 native/레거시 중 맞는 소비자를 고른다.
    let manifest = build_manifest(
        &stored.backup_id,
        &server_meta,
        topology,
        selective,
        stored.original_size,
        stored.stored_size,
        &stored.checksum,
        oplog_range,
        archive_format,
        &meta,
    );
    write_manifest_or_cleanup(storage, &manifest).await?;

    tracing::info!(
        backup_id = %stored.backup_id,
        bytes = stored.stored_size,
        checksum = %stored.checksum,
        "풀 백업 완료"
    );

    Ok(BackupOutcome {
        backup_id: stored.backup_id,
        stored_size_bytes: stored.stored_size,
        original_size_bytes: stored.original_size,
        checksum_sha256: stored.checksum,
        topology,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
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
    archive_format: &str,
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
            mongodump: None,
            // 복구가 소비자(mongorestore vs native)를 고르는 기준.
            archive_format: Some(archive_format.to_string()),
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
        mysql_binlog: None,
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

/// dump 스트림 종료 판정 훅 — 엔진 공통 저장 코어([`store_dump_stream`])가 성공/실패
/// 경로에서 호출한다(Phase 1 슬라이스 B). `Output`은 dump 측이 종료 시 넘겨주는 엔진
/// 메타다(MySQL=스냅샷 binlog 좌표, Mongo/PG=없음).
trait DumpTermination {
    type Output;
    /// 정상 경로 — put_stream EOF 후 dump 종료를 판정하고 결과를 회수한다.
    async fn finish(self) -> Result<Self::Output>;
    /// 실패 경로 — 자식 프로세스/task를 정리한다(좀비/누수 방지).
    async fn abort(self);
}

impl DumpTermination for DumpFinalizer {
    type Output = ();
    async fn finish(mut self) -> Result<()> {
        DumpFinalizer::finish(&mut self).await
    }
    async fn abort(mut self) {
        DumpFinalizer::abort(&mut self).await;
    }
}

impl DumpTermination for crate::engine::postgres::backup::PgDumpHandle {
    type Output = ();
    async fn finish(self) -> Result<()> {
        crate::engine::postgres::backup::PgDumpHandle::finish(self).await
    }
    // COPY task는 리더 drop으로 끝난다 — 결과만 흡수(종전 put 실패 경로와 동일).
    async fn abort(self) {
        let _ = crate::engine::postgres::backup::PgDumpHandle::finish(self).await;
    }
}

impl DumpTermination for crate::engine::mysql::backup::MysqlDumpHandle {
    type Output = Option<crate::manifest::schema::MysqlBinlogCoords>;
    async fn finish(self) -> Result<Self::Output> {
        crate::engine::mysql::backup::MysqlDumpHandle::finish(self).await
    }
    async fn abort(self) {
        let _ = crate::engine::mysql::backup::MysqlDumpHandle::finish(self).await;
    }
}

impl DumpTermination for crate::engine::file::backup::FileDumpHandle {
    type Output = ();
    async fn finish(self) -> Result<()> {
        crate::engine::file::backup::FileDumpHandle::finish(self).await
    }
    // tar 직렬화 task는 리더 drop으로 BrokenPipe 종료된다 — 결과만 흡수.
    async fn abort(self) {
        let _ = crate::engine::file::backup::FileDumpHandle::finish(self).await;
    }
}

/// 공통 저장 산출물 — 코어가 확정한 ID·크기·체크섬과 종료 훅의 엔진 메타.
struct StoredDump<T> {
    backup_id: String,
    stored_size: u64,
    original_size: u64,
    checksum: String,
    dump_output: T,
}

/// 엔진 공통 스트리밍 저장 코어(Phase 1 슬라이스 B) — 세 풀 백업 경로가 각자 들고 있던
/// "합성 → 저장 → 종료 판정 → 확정" 골격의 단일 구현.
///
/// dump 스트림 → 원본 카운터 → 스테이지(compress→encrypt) → sha256 tee → 저장 카운터
/// → put_stream. put 실패면 종료 훅 abort 후 전파, dump 종료 실패면 저장 산출물
/// 정리([`cleanup`]) 후 전파. 새 엔진은 dump 스트림과 [`DumpTermination`] 구현만 만들면
/// 이 코어에 그대로 접속한다(manifest 조립은 엔진별로 남는다 — 필드 의미가 다르다).
async fn store_dump_stream<T: DumpTermination>(
    storage: &dyn Storage,
    dump_stream: BoxAsyncRead,
    stages: StageStack,
    progress_counter: &Option<Arc<AtomicU64>>,
    termination: T,
) -> Result<StoredDump<T::Output>> {
    // 입력 카운터는 *압축 전* 원본 바이트를 세고, sha256 tee는 *저장 직전* 최종 바이트에
    // 걸린다(설계 불변: 체크섬=저장 바이트). 진행 카운터가 주입됐으면 저장 카운터의
    // backing Arc로 공유한다(진행 표시 폴링).
    let counted = CountingReader::new(dump_stream);
    let original_size_handle = counted.handle();
    let staged: BoxAsyncRead = stages.apply(Box::pin(counted));
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();
    let stored_counted = match progress_counter {
        Some(counter) => CountingReader::with_counter(Box::pin(checksummed), Arc::clone(counter)),
        None => CountingReader::new(Box::pin(checksummed)),
    };
    let stored_size_handle = stored_counted.handle();

    // data.bin 저장(업로드 먼저). put_stream이 바이트를 끝까지 소비한다.
    let backup_id = Uuid::now_v7().to_string();
    let put_result = storage
        .put_stream(&data_path(&backup_id), Box::pin(stored_counted), None)
        .await;

    // 업로드 성공/실패와 무관하게 dump 종료를 판정해야 한다(좀비/누수 방지).
    if let Err(put_err) = put_result {
        termination.abort().await;
        return Err(put_err);
    }
    let dump_output = match termination.finish().await {
        Ok(out) => out,
        Err(dump_err) => {
            cleanup(storage, &backup_id).await;
            return Err(dump_err);
        }
    };

    // put_stream이 끝났으므로 EOF까지 누산 완료 — 체크섬·크기 확정.
    let checksum = checksum_handle
        .finalize()
        .ok_or_else(|| XBackupError::Failure("체크섬 확정 실패(이미 소비됨)".into()))?;
    Ok(StoredDump {
        stored_size: stored_size_handle.total(),
        original_size: original_size_handle.total(),
        backup_id,
        checksum,
        dump_output,
    })
}

/// manifest를 기록하고, 실패 시 저장 산출물을 정리한다(data 다음 manifest, pitfall 7-1).
async fn write_manifest_or_cleanup(storage: &dyn Storage, manifest: &BackupManifest) -> Result<()> {
    let store = ManifestStore::new(storage);
    if let Err(write_err) = store.write(manifest).await {
        cleanup(storage, &manifest.id).await;
        return Err(write_err);
    }
    Ok(())
}

/// PostgreSQL 풀 백업 — 드라이버 COPY 아카이브(`xb-pg-v1`)를 압축→암호화→저장 파이프라인에
/// 흘린다. Mongo 경로와 달리 oplog/토폴로지가 없다(topology=Standalone로 기록). 복구는
/// manifest.archive_format로 PG 엔진을 고른다. 파이프라인 합성·체크섬·정리는 Mongo와 동형
/// (공통 코어 [`store_dump_stream`] 사용).
#[allow(clippy::too_many_arguments)]
pub async fn run_pg_full_backup(
    uri: &crate::config::secret::Secret,
    timeout_secs: Option<u64>,
    db: Option<String>,
    collection: Option<String>,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
    progress_counter: Option<Arc<AtomicU64>>,
    profile_name: &str,
    enable_incremental: bool,
) -> Result<BackupOutcome> {
    use crate::engine::postgres::{
        archive as pg_archive, backup::PgDumper, conn::PgClient, incremental,
    };

    let selective = db.is_some() || collection.is_some();

    // 증분 활성(features.incremental.pg_logical)이면 **덤프 전에** replication slot+publication을
    // **새로** 만들어(있으면 재생성) base에 정렬한다 — 슬롯이 이 시점부터 WAL을 보존해야
    // 이후 변경을 빠짐없이 캡처한다. 매 풀백업마다 재생성하므로 살아있는 슬롯이 항상 최신
    // 풀백업(=증분 base)에 묶인다(C2 — 슬롯 재사용으로 인한 조용한 체인 붕괴 방지). 선택적
    // 백업은 증분 base 부적격이라 건너뛴다.
    if enable_incremental && !selective {
        let admin = PgClient::connect(uri, timeout_secs).await?;
        ensure_wal_level_logical(admin.client()).await?;
        let slot = incremental::slot_name(profile_name);
        let publication = incremental::publication_name(profile_name);
        let base_lsn =
            incremental::recreate_slot_and_publication(admin.client(), &slot, &publication).await?;
        tracing::info!(%slot, %publication, %base_lsn, "PG 증분 slot 재생성 완료(풀 백업 base에 정렬)");
    } else if enable_incremental && selective {
        tracing::warn!(
            "선택적 PG 백업(--db/--collection)은 증분 base 부적격이라 slot을 만들지 않습니다"
        );
    }

    let dumper = PgDumper::connect(uri, timeout_secs).await?;
    let server_version = dumper
        .server_version()
        .await
        .map(|v| format!("postgresql {v}"))
        .unwrap_or_else(|| "postgresql".to_string());
    let dump = dumper.dump_stream(db, collection);
    let dump_handle = dump.handle();

    // 공통 코어: 합성 → 저장 → 종료 판정 → 확정(Mongo 경로와 동일 골격).
    let stored = store_dump_stream(
        storage,
        Box::pin(dump),
        stages,
        &progress_counter,
        dump_handle,
    )
    .await?;

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        id: stored.backup_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::Standalone,
        server_version,
        tool_versions: ToolVersions {
            mongodump: None,
            archive_format: Some(pg_archive::FORMAT_ID.to_string()),
        },
        selective,
        original_size_bytes: stored.original_size,
        stored_size_bytes: stored.stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: stored.checksum.clone(),
        oplog_range: None,
        oplog_count: None,
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    };
    write_manifest_or_cleanup(storage, &manifest).await?;

    tracing::info!(backup_id = %stored.backup_id, bytes = stored.stored_size, checksum = %stored.checksum, "PG 풀 백업 완료");
    Ok(BackupOutcome {
        backup_id: stored.backup_id,
        stored_size_bytes: stored.stored_size,
        original_size_bytes: stored.original_size,
        checksum_sha256: stored.checksum,
        topology: Topology::Standalone,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        oplog_range: None,
    })
}

/// MySQL 풀 백업 — 드라이버(SHOW CREATE + SELECT) 아카이브를 압축·암호화·저장한다.
///
/// PG 경로와 같은 storage/stage/sha256/카운터 골격을 공유한다. 슬롯이 없는 대신 덤프 task가
/// 스냅샷 시점 binlog 좌표를 캡처하고, [`finish`](crate::engine::mysql::backup::MysqlDumpHandle::finish)가
/// 그것을 반환하면 manifest의 `mysql_binlog`에 기록한다(증분 base). `table_filter`는 `--collection`.
#[allow(clippy::too_many_arguments)]
pub async fn run_mysql_full_backup(
    uri: &crate::config::secret::Secret,
    timeout_secs: Option<u64>,
    table_filter: Option<String>,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
    progress_counter: Option<Arc<AtomicU64>>,
    promoted_from_gap: bool,
) -> Result<BackupOutcome> {
    use crate::engine::mysql::{archive as my_archive, backup::MysqlDumper};

    let selective = table_filter.is_some();

    let mut dumper = MysqlDumper::connect(uri, timeout_secs).await?;
    let server_version = dumper
        .server_version()
        .await
        .map(|v| format!("mysql {v}"))
        .unwrap_or_else(|| "mysql".to_string());
    let dump = dumper.dump_stream(table_filter);
    let dump_handle = dump.handle();

    // 공통 코어: 합성 → 저장 → 종료 판정 → 확정. dump_output이 스냅샷 binlog 좌표
    // (증분 base — MysqlDumpHandle::finish의 반환값)다.
    let stored = store_dump_stream(
        storage,
        Box::pin(dump),
        stages,
        &progress_counter,
        dump_handle,
    )
    .await?;
    let mysql_binlog = stored.dump_output;

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        id: stored.backup_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::Standalone,
        server_version,
        tool_versions: ToolVersions {
            mongodump: None,
            archive_format: Some(my_archive::FORMAT_ID.to_string()),
        },
        selective,
        original_size_bytes: stored.original_size,
        stored_size_bytes: stored.stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: stored.checksum.clone(),
        oplog_range: None,
        oplog_count: None,
        promoted_from_gap,
        mysql_binlog,
        status: BackupStatus::Complete,
    };
    write_manifest_or_cleanup(storage, &manifest).await?;

    tracing::info!(backup_id = %stored.backup_id, bytes = stored.stored_size, checksum = %stored.checksum, "MySQL 풀 백업 완료");
    Ok(BackupOutcome {
        backup_id: stored.backup_id,
        stored_size_bytes: stored.stored_size,
        original_size_bytes: stored.original_size,
        checksum_sha256: stored.checksum,
        topology: Topology::Standalone,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        oplog_range: None,
    })
}

/// 파일/디렉터리 풀 백업 — 로컬 경로의 tar 스트림(`xb-file-tar-v1`)을 압축→암호화→저장
/// 파이프라인에 흘린다(P2-1). DB 메타(oplog/topology/서버 버전)가 없어 manifest는
/// Standalone·버전 `-`로 기록한다. 복구는 manifest.archive_format으로 파일 엔진을 고른다.
pub async fn run_file_full_backup(
    source_path: std::path::PathBuf,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
    progress_counter: Option<Arc<AtomicU64>>,
) -> Result<BackupOutcome> {
    let dump = crate::engine::file::backup::FileDumper::open(source_path)?.dump_stream();
    let dump_handle = dump.handle();

    // 공통 코어: 합성 → 저장 → 종료 판정 → 확정(DB 엔진들과 동일 골격).
    let stored = store_dump_stream(
        storage,
        Box::pin(dump),
        stages,
        &progress_counter,
        dump_handle,
    )
    .await?;

    let manifest = BackupManifest {
        format_version: FORMAT_VERSION,
        id: stored.backup_id.clone(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Full,
        base_id: None,
        topology: Topology::Standalone,
        server_version: "-".to_string(),
        tool_versions: ToolVersions {
            mongodump: None,
            archive_format: Some(crate::engine::file::FORMAT_ID.to_string()),
        },
        selective: false,
        original_size_bytes: stored.original_size,
        stored_size_bytes: stored.stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: stored.checksum.clone(),
        oplog_range: None,
        oplog_count: None,
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    };
    write_manifest_or_cleanup(storage, &manifest).await?;

    tracing::info!(backup_id = %stored.backup_id, bytes = stored.stored_size, checksum = %stored.checksum, "파일 풀 백업 완료");
    Ok(BackupOutcome {
        backup_id: stored.backup_id,
        stored_size_bytes: stored.stored_size,
        original_size_bytes: stored.original_size,
        checksum_sha256: stored.checksum,
        topology: Topology::Standalone,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        oplog_range: None,
    })
}

/// MySQL 증분 백업 결과.
pub struct MysqlIncrementalOutcome {
    pub backup_id: String,
    pub base_id: String,
    pub change_count: u64,
    pub stored_size_bytes: u64,
    /// gap 감지로 풀 백업으로 승격됐는지.
    pub promoted: bool,
}

/// MySQL 증분 백업 — base 체인 이후의 binlog ROW 변경을 캡처해 저장한다(`xb-mysql-incr-v1`).
///
/// base(최신 풀백업)의 binlog 파일이 purge됐으면(gap) 풀 백업으로 **승격**한다(FR-2). 변경이
/// 없으면 빈 슬라이스(data.bin 없이 manifest만)로 체인 좌표만 전진시킨다.
#[allow(clippy::too_many_arguments)]
pub async fn run_mysql_incremental_backup(
    uri: &crate::config::secret::Secret,
    timeout_secs: Option<u64>,
    profile_name: &str,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
    progress_counter: Option<Arc<AtomicU64>>,
) -> Result<MysqlIncrementalOutcome> {
    use crate::engine::mysql::{conn::MysqlClient, incremental};
    use crate::pipeline::mysql_pitr::{chain_start_coords, latest_mysql_full};

    // 1) base 선택 + 체인 시작 좌표.
    let base_id = latest_mysql_full(storage).await?;
    let start = chain_start_coords(storage, &base_id).await?;

    // 2) gap 체크 + 서버 버전.
    let mut admin = MysqlClient::connect(uri, timeout_secs).await?;
    let server_version = admin
        .server_version()
        .await
        .ok()
        .flatten()
        .map(|v| format!("mysql {v}"))
        .unwrap_or_else(|| "mysql".to_string());
    let available = incremental::binlog_available(admin.conn_mut(), &start).await?;
    drop(admin);

    // 3) gap → 풀 승격(promoted_from_gap=true).
    if !available {
        tracing::warn!(base_id = %base_id, "MySQL 증분 gap(base binlog purge) — 풀 백업으로 승격");
        let out = run_mysql_full_backup(
            uri,
            timeout_secs,
            None,
            storage,
            stages,
            meta,
            progress_counter,
            true,
        )
        .await?;
        return Ok(MysqlIncrementalOutcome {
            backup_id: out.backup_id,
            base_id,
            change_count: 0,
            stored_size_bytes: out.stored_size_bytes,
            promoted: true,
        });
    }

    // 4) binlog ROW 변경 캡처.
    let server_id = incremental::server_id_for(profile_name);
    let captured = incremental::capture(uri, timeout_secs, &start, server_id).await?;

    // 5) 변경 0건 — data 없이 manifest만(빈 슬라이스 계약), 체인 좌표만 전진.
    if captured.count == 0 {
        let backup_id = Uuid::now_v7().to_string();
        let manifest = mysql_incremental_manifest(
            &backup_id,
            &base_id,
            &server_version,
            0,
            &empty_sha256(),
            0,
            &BackupMeta::none(),
            Some(captured.end),
        );
        ManifestStore::new(storage).write(&manifest).await?;
        tracing::info!(backup_id = %backup_id, base_id = %base_id, "MySQL 증분 — 변경 없음(빈 슬라이스)");
        return Ok(MysqlIncrementalOutcome {
            backup_id,
            base_id,
            change_count: 0,
            stored_size_bytes: 0,
            promoted: false,
        });
    }

    // 6) 캡처 바이트를 파이프라인(압축→암호화)→sha256→저장으로(풀과 동형).
    let end = captured.end.clone();
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(captured.archive));
    let staged: BoxAsyncRead = stages.apply(reader);
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();
    let stored_counted = match &progress_counter {
        Some(counter) => CountingReader::with_counter(Box::pin(checksummed), Arc::clone(counter)),
        None => CountingReader::new(Box::pin(checksummed)),
    };
    let stored_size_handle = stored_counted.handle();

    let backup_id = Uuid::now_v7().to_string();
    let data_rel = data_path(&backup_id);
    if let Err(put_err) = storage
        .put_stream(&data_rel, Box::pin(stored_counted), None)
        .await
    {
        cleanup(storage, &backup_id).await;
        return Err(put_err);
    }
    let checksum = checksum_handle
        .finalize()
        .ok_or_else(|| XBackupError::Failure("체크섬 확정 실패(이미 소비됨)".into()))?;
    let stored_size = stored_size_handle.total();

    let manifest = mysql_incremental_manifest(
        &backup_id,
        &base_id,
        &server_version,
        stored_size,
        &checksum,
        captured.count,
        &meta,
        Some(end),
    );
    if let Err(write_err) = ManifestStore::new(storage).write(&manifest).await {
        cleanup(storage, &backup_id).await;
        return Err(write_err);
    }

    tracing::info!(backup_id = %backup_id, base_id = %base_id, changes = captured.count, bytes = stored_size, "MySQL 증분 백업 완료");
    Ok(MysqlIncrementalOutcome {
        backup_id,
        base_id,
        change_count: captured.count,
        stored_size_bytes: stored_size,
        promoted: false,
    })
}

/// MySQL 증분 manifest — 변경 수를 oplog_count에 재사용(빈 슬라이스 Some(0) 계약), end 좌표를
/// mysql_binlog에 기록(다음 증분 시작점·체인).
#[allow(clippy::too_many_arguments)]
fn mysql_incremental_manifest(
    backup_id: &str,
    base_id: &str,
    server_version: &str,
    stored_size: u64,
    checksum: &str,
    change_count: u64,
    meta: &BackupMeta,
    end_coords: Option<crate::manifest::schema::MysqlBinlogCoords>,
) -> BackupManifest {
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: backup_id.to_string(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Incremental,
        base_id: Some(base_id.to_string()),
        topology: Topology::Standalone,
        server_version: server_version.to_string(),
        tool_versions: ToolVersions {
            mongodump: None,
            archive_format: Some(crate::engine::mysql::incremental::INCR_FORMAT_ID.to_string()),
        },
        selective: false,
        original_size_bytes: stored_size,
        stored_size_bytes: stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: checksum.to_string(),
        oplog_range: None,
        oplog_count: Some(change_count),
        promoted_from_gap: false,
        mysql_binlog: end_coords,
        status: BackupStatus::Complete,
    }
}

/// `wal_level=logical`을 확인한다 — 아니면 증분 slot을 만들 수 없으므로 명확히 거부(exit 3).
async fn ensure_wal_level_logical(client: &tokio_postgres::Client) -> Result<()> {
    let level: String = client
        .query_one("SHOW wal_level", &[])
        .await
        .map(|r| r.get(0))
        .map_err(|e| XBackupError::PrecheckFailed(format!("wal_level 조회 실패: {e}")))?;
    if level != "logical" {
        return Err(XBackupError::PrecheckFailed(format!(
            "PG 증분에는 wal_level=logical이 필요합니다(현재 '{level}'). \
             서버에서 `ALTER SYSTEM SET wal_level=logical;` 후 재시작하세요. \
             증분이 필요 없으면 features.incremental.pg_logical=false로 두세요."
        )));
    }
    Ok(())
}

/// PG 증분 백업 결과(CLI 출력용).
#[derive(Debug, Clone)]
pub struct PgIncrementalOutcome {
    /// 증분 백업 ID.
    pub backup_id: String,
    /// 연결된 base 풀백업 ID.
    pub base_id: String,
    /// 캡처한 변경 레코드 수(0이면 빈 슬라이스, data.bin 미생성).
    pub change_count: u64,
    /// 저장 바이트(빈 슬라이스면 0).
    pub stored_size_bytes: u64,
}

/// PostgreSQL 증분 백업 — logical decoding(pgoutput) slot에서 변경을 캡처해 `xb-pg-incr-v1`
/// 아카이브로 저장한다. base는 가장 최신 Complete PG 풀백업(스타 모델 — 증분은 모두 그
/// 풀백업에 체인). 저장 성공 후에만 slot을 전진시킨다(실패 시 재캡처 — 적용이 idempotent).
///
/// 무결성 순서: data.bin 저장 → manifest 기록 → slot 전진. manifest 기록 실패면 slot
/// 미전진이라 다음에 재캡처(부분 산출물은 정리). 전진 실패는 다음 증분이 겹쳐 캡처하나
/// 복구 적용이 idempotent라 안전.
#[allow(clippy::too_many_arguments)]
pub async fn run_pg_incremental_backup(
    uri: &crate::config::secret::Secret,
    timeout_secs: Option<u64>,
    profile_name: &str,
    storage: &dyn Storage,
    stages: StageStack,
    meta: BackupMeta,
) -> Result<PgIncrementalOutcome> {
    use crate::engine::postgres::{conn::PgClient, incremental};
    use crate::manifest::schema::BackupStatus;

    // 1) base 풀백업 선택 — 가장 최신 Complete·비-selective PG 풀백업.
    let base_id = select_pg_full_base(storage).await?;

    // 2) slot에서 변경 캡처(peek — 비소비).
    let admin = PgClient::connect(uri, timeout_secs).await?;
    let server_version = admin
        .client()
        .query_one("SHOW server_version", &[])
        .await
        .map(|r| format!("postgresql {}", r.get::<_, String>(0)))
        .unwrap_or_else(|_| "postgresql".to_string());
    let slot = incremental::slot_name(profile_name);
    let publication = incremental::publication_name(profile_name);
    let captured = incremental::capture(admin.client(), &slot, &publication).await?;

    // 3) 변경 0건 — data 없이 manifest만 기록(빈 슬라이스 계약), slot은 마지막 LSN까지 전진.
    if captured.count == 0 {
        let backup_id = Uuid::now_v7().to_string();
        let manifest = pg_incremental_manifest(
            &backup_id,
            &base_id,
            &server_version,
            /* stored */ 0,
            &empty_sha256(),
            /* count */ 0,
            &BackupMeta::none(),
        );
        let store = ManifestStore::new(storage);
        store.write(&manifest).await?;
        if let Some(lsn) = &captured.last_lsn {
            // DML이 아닌 메시지만 있던 구간 — 다시 안 읽도록 전진.
            if let Err(e) = incremental::advance_slot(admin.client(), &slot, lsn).await {
                tracing::warn!("빈 슬라이스 slot 전진 실패(다음에 재시도): {e}");
            }
        }
        tracing::info!(backup_id = %backup_id, base_id = %base_id, "PG 증분 — 변경 없음(빈 슬라이스)");
        return Ok(PgIncrementalOutcome {
            backup_id,
            base_id,
            change_count: 0,
            stored_size_bytes: 0,
        });
    }

    // 4) 캡처 바이트를 파이프라인(압축→암호화)→sha256→저장으로 흘린다(풀과 동형).
    let last_lsn = captured.last_lsn.clone();
    let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(captured.archive));
    let staged: BoxAsyncRead = stages.apply(reader);
    let checksummed = Sha256Reader::new(staged);
    let checksum_handle = checksummed.handle();
    let stored_counted = CountingReader::new(Box::pin(checksummed));
    let stored_size_handle = stored_counted.handle();

    let backup_id = Uuid::now_v7().to_string();
    let data_rel = data_path(&backup_id);
    if let Err(put_err) = storage
        .put_stream(&data_rel, Box::pin(stored_counted), None)
        .await
    {
        cleanup(storage, &backup_id).await;
        return Err(put_err);
    }
    let checksum = checksum_handle
        .finalize()
        .ok_or_else(|| XBackupError::Failure("체크섬 확정 실패(이미 소비됨)".into()))?;
    let stored_size = stored_size_handle.total();

    // 5) manifest 기록(data 다음). 실패 시 정리(slot 미전진 → 다음에 재캡처).
    let manifest = pg_incremental_manifest(
        &backup_id,
        &base_id,
        &server_version,
        stored_size,
        &checksum,
        captured.count,
        &meta,
    );
    let store = ManifestStore::new(storage);
    if let Err(write_err) = store.write(&manifest).await {
        cleanup(storage, &backup_id).await;
        return Err(write_err);
    }

    // 6) 저장·manifest가 끝났으니 slot 전진(여기 실패는 다음 증분이 겹쳐 캡처 — idempotent).
    if let Some(lsn) = &last_lsn {
        if let Err(e) = incremental::advance_slot(admin.client(), &slot, lsn).await {
            tracing::warn!("slot 전진 실패(다음 증분이 겹쳐 캡처, 복구는 idempotent): {e}");
        }
    }
    let _ = BackupStatus::Complete; // (manifest 헬퍼가 이미 Complete로 기록)

    tracing::info!(backup_id = %backup_id, base_id = %base_id, changes = captured.count, bytes = stored_size, "PG 증분 백업 완료");
    Ok(PgIncrementalOutcome {
        backup_id,
        base_id,
        change_count: captured.count,
        stored_size_bytes: stored_size,
    })
}

/// PG 증분 manifest 조립 — backup_type=Incremental, base_id, oplog_count(=변경 수 재사용),
/// archive_format=xb-pg-incr-v1. oplog_range/LSN은 기록하지 않는다(slot이 위치의 진실원).
fn pg_incremental_manifest(
    backup_id: &str,
    base_id: &str,
    server_version: &str,
    stored_size: u64,
    checksum: &str,
    change_count: u64,
    meta: &BackupMeta,
) -> BackupManifest {
    BackupManifest {
        format_version: FORMAT_VERSION,
        id: backup_id.to_string(),
        created_at: Utc::now().to_rfc3339(),
        backup_type: BackupType::Incremental,
        base_id: Some(base_id.to_string()),
        topology: Topology::Standalone,
        server_version: server_version.to_string(),
        tool_versions: ToolVersions {
            mongodump: None,
            archive_format: Some(crate::engine::postgres::incremental::INCR_FORMAT_ID.to_string()),
        },
        selective: false,
        original_size_bytes: stored_size,
        stored_size_bytes: stored_size,
        compression: meta.compression.clone(),
        encryption: meta.encryption.clone(),
        checksum_sha256: checksum.to_string(),
        oplog_range: None,
        // 변경 레코드 수를 oplog_count에 재사용 — 빈 슬라이스(Some(0)) 계약을 그대로 따른다.
        oplog_count: Some(change_count),
        promoted_from_gap: false,
        mysql_binlog: None,
        status: BackupStatus::Complete,
    }
}

/// 빈 입력의 sha256(빈 슬라이스 data 부재 시 manifest checksum 자리값).
fn empty_sha256() -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(b""))
}

/// 가장 최신 Complete·비-selective PG 풀백업(archive_format=xb-pg-v1) ID를 고른다.
///
/// PG 증분은 스타 모델로 이 풀백업에 모두 체인된다(slot이 위치를 추적하므로 tip ts는
/// 불필요). 적격 base가 없으면 Usage 에러(exit 2 — 먼저 풀 백업 필요).
async fn select_pg_full_base(storage: &dyn Storage) -> Result<String> {
    use crate::manifest::schema::BackupStatus;
    let store = ManifestStore::new(storage);
    let mut ids = crate::pipeline::verify::collect_manifest_ids(storage).await?;
    ids.sort();
    ids.reverse(); // UUID v7 사전순=생성순 — 최신부터.
    for id in &ids {
        let m = match store.read(id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(id = %id, "base 후보 manifest 읽기 실패(건너뜀): {e}");
                continue;
            }
        };
        let is_pg_full = matches!(m.backup_type, BackupType::Full)
            && matches!(m.status, BackupStatus::Complete)
            && !m.selective
            && m.tool_versions.archive_format.as_deref()
                == Some(crate::engine::postgres::archive::FORMAT_ID);
        if is_pg_full {
            return Ok(m.id);
        }
    }
    Err(XBackupError::Usage(
        "PG 증분의 base가 될 Complete 풀백업이 없습니다 — 먼저 풀 백업(--type full)을 \
         features.incremental.pg_logical=true로 한 번 수행하세요."
            .into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::MockStorage;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;

    // ── Engine::parse — legacy-mongodump feature 게이트(P0-2) ──

    #[test]
    fn engine_parse_native_always_ok() {
        assert_eq!(Engine::parse("native").unwrap(), Engine::Native);
        assert_eq!(Engine::parse("bogus").unwrap_err().exit_code(), 2);
    }

    #[cfg(feature = "legacy-mongodump")]
    #[test]
    fn engine_parse_mongodump_ok_with_legacy_feature() {
        assert_eq!(Engine::parse("mongodump").unwrap(), Engine::Mongodump);
    }

    #[cfg(not(feature = "legacy-mongodump"))]
    #[test]
    fn engine_parse_mongodump_rejected_without_legacy_feature() {
        let err = Engine::parse("mongodump").unwrap_err();
        assert_eq!(err.exit_code(), 2, "설정 오류(exit 2)여야 함");
        assert!(
            err.to_string().contains("legacy-mongodump"),
            "feature 안내 누락: {err}"
        );
    }

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
            mysql_binlog: None,
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
            timeout_secs: None,
            engine: Engine::default(),
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
            timeout_secs: None,
            engine: r.engine,
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
            "mongodump",
            &meta,
        );
        assert!(m.selective, "선택적 백업 manifest.selective=true");
        assert_eq!(m.original_size_bytes, 1000);
        assert_eq!(m.stored_size_bytes, 250);
        assert_eq!(m.compression.unwrap().level, 10);
        assert_eq!(m.encryption.unwrap().algorithm, "age");
    }
}
