//! 풀 복구 파이프라인 — storage 읽기 → 역스택 → mongorestore stdin 스트리밍(PRD §FR-3).
//!
//! 백업([`super::backup`])의 정확한 역방향이다(PRD §7 "복구 역방향"):
//! ```text
//! Storage get_stream(data.bin) ─▶ reverse_stack(decrypt→decompress) ─▶ mongorestore --archive=- stdin
//! ```
//! t5는 평문 경로(reverse 스택 = identity)만 동작한다. 압축·암호화 역단계는 t6이
//! [`reverse_stack_for`](super::stage::reverse_stack_for)에 채운다 — 이 파이프라인은
//! 그 합성 결과를 소비할 뿐 한 줄도 바뀌지 않는다.
//!
//! ## 안전 설계(PRD §FR-3 가드레일)
//! - **복구 사전 점검(R7):** 대상 URI 연결 확인 → 서버 버전 vs manifest.server_version
//!   호환 경고 → 기존 사용자 데이터(복원 대상 네임스페이스) 존재 감지. 연결 실패는
//!   [`XBackupError::PrecheckFailed`](exit 3). `--skip-precheck`로 우회.
//! - **가드레일(R5):** 기존 데이터가 있고 `--force`가 없으면 — TTY면 대화형 확인,
//!   비-TTY면 거부([`XBackupError::Failure`], exit 1). `--drop`은 가드 통과 시에만 켠다.
//! - **dry-run:** backup id·대상 URI(redacted)·예상 크기·충돌 네임스페이스를 [`RestorePlan`]
//!   으로 만들어 무변경 종료(exit 0). 실제 mongorestore를 스폰하지 않는다.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use tokio::io::{AsyncRead, ReadBuf};

use std::path::{Path, PathBuf};

use crate::engine::mongo::meta::ServerMeta;
use crate::engine::mongo::MongoMeta;
#[cfg(feature = "legacy-mongodump")]
use crate::engine::mongo::{RestoreProcess, RestoreSpec, UriConfigFile};
use crate::engine::native::{native_export_to_dir, ExportSummary};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{BackupManifest, BackupType};
use crate::manifest::store::{data_path, ManifestStore, MANIFEST_FILE};
use crate::pipeline::stage::reverse_stack_for;
use crate::storage::{BoxAsyncRead, Storage};

/// 통과 바이트를 공유 카운터에 누산하는 [`AsyncRead`] 래퍼(진행 표시용 — t13/R16).
///
/// 복구 입력 스트림(역스택 출력)을 감싸 mongorestore stdin으로 흐르는 바이트를 센다.
/// 카운터는 진행 표시기가 폴링하는 `Arc<AtomicU64>`와 공유된다(`poll_read`에서 fetch_add).
/// 핸들러가 [`run_restore_with_progress`]로 카운터를 주입할 때만 삽입된다.
struct ProgressCountingReader {
    inner: BoxAsyncRead,
    counter: Arc<AtomicU64>,
}

impl ProgressCountingReader {
    fn new(inner: BoxAsyncRead, counter: Arc<AtomicU64>) -> Self {
        Self { inner, counter }
    }
}

impl AsyncRead for ProgressCountingReader {
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

/// 복구 요청.
pub struct RestoreRequest {
    /// 복구 대상 MongoDB URI 시크릿(`--target` 우선, 없으면 프로파일 source).
    pub target_uri: crate::config::secret::Secret,
    /// mongorestore 실행파일 경로(보통 `"mongorestore"`).
    pub mongorestore_program: String,
    /// 복구할 백업 ID(`--id`). None이면 최신 풀 백업을 자동 선택한다.
    pub backup_id: Option<String>,
    /// 선택적 복구 대상(`--only db.collection`). `--nsInclude`로 전달된다.
    pub only: Option<String>,
    /// 기존 데이터 덮어쓰기 가드 해제(`--force`).
    pub force: bool,
    /// 복구 계획만 출력(`--dry-run`) — 실제 복원 안 함.
    pub dry_run: bool,
    /// 복구 사전 점검 우회(`--skip-precheck`).
    pub skip_precheck: bool,
    /// MongoDB 접속 타임아웃(초). `None`이면 기본 5초.
    pub timeout_secs: Option<u64>,
    /// 진행 표시용 공유 바이트 카운터(t13/R16). `Some`이면 mongorestore stdin으로 흘리는
    /// 복원 입력 바이트를 누산해, 핸들러의 진행 표시기가 폴링한다(없으면 미주입 — 무비용).
    pub progress_counter: Option<Arc<AtomicU64>>,
}

/// 복구 계획(dry-run 출력·실행 요약 공통). 시크릿은 담지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    /// 사용할 백업 ID.
    pub backup_id: String,
    /// 백업 유형(full/incremental).
    pub backup_type: BackupType,
    /// 백업 원본 서버 버전(manifest 기록).
    pub source_server_version: String,
    /// 예상 복원 입력 크기(저장 바이트 = data.bin). 압축 시 실제 복원량은 더 클 수 있다.
    pub stored_size_bytes: u64,
    /// 선택적 복구 네임스페이스 필터(`--only`).
    pub ns_include: Option<String>,
    /// 복원 대상에 이미 존재해 **충돌**하는 사용자 네임스페이스(없으면 빈 벡터).
    pub conflicting_namespaces: Vec<String>,
    /// 대상 서버 버전(점검 시 조회; skip-precheck면 None).
    pub target_server_version: Option<String>,
    /// 서버 버전 호환 경고 메시지(있을 때만).
    pub version_warning: Option<String>,
}

impl RestorePlan {
    /// 충돌(기존 데이터)이 있는지.
    pub fn has_conflicts(&self) -> bool {
        !self.conflicting_namespaces.is_empty()
    }
}

/// 복구 결과 요약(CLI 출력용).
#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    /// 복구에 사용한 백업 ID.
    pub backup_id: String,
    /// 스트리밍한 입력 바이트 수(data.bin 저장 크기).
    pub stored_size_bytes: u64,
    /// 복구 계획(요약·로깅).
    pub plan: RestorePlan,
}

/// 복구 계획을 수립한다 — 백업 선택 + manifest 로드 + (옵션) 사전 점검·충돌 감지.
///
/// dry-run·실제 복구가 공통으로 사용한다. mongorestore는 **스폰하지 않는다**.
/// `precheck_meta`가 `Some`이면 대상 서버에 질의해 버전 호환·충돌 네임스페이스를 채운다
/// (None이면 skip-precheck — 충돌 목록은 빈 채로 둔다).
async fn build_plan(
    request: &RestoreRequest,
    storage: &dyn Storage,
    precheck_meta: Option<&MongoMeta>,
) -> Result<RestorePlan> {
    // 1) 백업 선택: --id 우선, 없으면 최신 풀 백업 자동 선택.
    let store = ManifestStore::new(storage);
    let manifest = match &request.backup_id {
        Some(id) => store.read(id).await?,
        None => latest_full_manifest(storage).await?,
    };

    // 2) 사전 점검(있으면): 대상 서버 버전 호환 경고 + 충돌 네임스페이스 감지.
    let (target_server_version, version_warning, conflicting_namespaces) = match precheck_meta {
        Some(meta) => {
            let server_meta = meta.server_meta().await.map_err(|e| {
                // 연결/권한 실패는 사전 점검 실패(exit 3) — 작업 미시작(PRD §FR-3).
                XBackupError::PrecheckFailed(format!("복구 대상 서버 점검 실패: {e}"))
            })?;
            let warning = version_compat_warning(&server_meta, &manifest);
            let conflicts = meta
                .user_namespaces(request.only.as_deref())
                .await
                .map_err(|e| XBackupError::PrecheckFailed(format!("기존 데이터 조회 실패: {e}")))?;
            (Some(server_meta.server_version), warning, conflicts)
        }
        None => (None, None, Vec::new()),
    };

    Ok(RestorePlan {
        backup_id: manifest.id.clone(),
        backup_type: manifest.backup_type,
        source_server_version: manifest.server_version.clone(),
        stored_size_bytes: manifest.stored_size_bytes,
        ns_include: request.only.clone(),
        conflicting_namespaces,
        target_server_version,
        version_warning,
    })
}

/// 풀 복구를 끝까지 실행한다(선택 → 점검 → 가드 → 스트리밍 복구).
///
/// `is_tty`는 대화형 확인 분기용(가드레일). 비-TTY에서 기존 데이터가 있고 `--force`가
/// 없으면 거부한다. `confirm`은 TTY 대화형 확인 콜백(테스트 주입용) — TTY가 아니면
/// 호출되지 않는다.
pub async fn run_restore<C>(
    request: &RestoreRequest,
    storage: &dyn Storage,
    is_tty: bool,
    confirm: C,
) -> Result<RestoreOutcome>
where
    C: FnOnce(&RestorePlan) -> bool,
{
    let target_kind = crate::engine::DbKind::from_uri(request.target_uri.expose());
    let is_pg = target_kind == crate::engine::DbKind::Postgres;
    let is_mysql = target_kind == crate::engine::DbKind::Mysql;
    // PG·MySQL은 드라이버 기반 SQL 복구라 Mongo 메타/가드 경로가 다르다(공통 처리).
    let is_driver = is_pg || is_mysql;

    // H6: 드라이버 복구(PG/MySQL)는 `--only`(선택적 복구)를 지원하지 않는다 — ns 필터가 없어
    //   전체를 복구하면서 계획만 좁게 보여주면 데이터 범위가 거짓 보고된다. 명확히 거부한다
    //   (exit 2). 시점 복구(--at) 경로도 동일하게 --only를 거부한다.
    if is_driver && request.only.is_some() {
        return Err(XBackupError::Usage(
            "PG/MySQL 복구는 --only(선택적 복구)를 지원하지 않습니다 — 전체 복구만 가능합니다. \
             특정 테이블만 필요하면 복구 후 정리하거나 별도 도구를 사용하세요."
                .into(),
        ));
    }

    // 사전 점검 메타(Mongo): skip-precheck가 아니면 대상에 연결해 점검에 활용한다.
    // dry-run도 충돌 목록을 보여주려면 점검이 필요하므로 동일하게 연결한다. PG는 드라이버가
    // 달라 Mongo 메타 경로를 타지 않고, 아래에서 별도로 충돌을 채운다(H5).
    let meta = if request.skip_precheck || is_driver {
        None
    } else {
        Some(
            MongoMeta::connect(&request.target_uri, request.timeout_secs)
                .await
                .map_err(|e| XBackupError::PrecheckFailed(format!("복구 대상 연결 실패: {e}")))?,
        )
    };

    let mut plan = build_plan(request, storage, meta.as_ref()).await?;

    // H5: PG 풀 복구도 프로덕션 덮어쓰기 가드레일을 적용한다 — 대상에 기존 사용자 테이블이
    //   있으면 충돌로 보고 `decide_guard`가 --force/대화형 확인/비-TTY 거부를 강제한다(FR-3).
    //   Mongo 메타 경로를 타지 않는 PG에서도 동일 가드가 걸리도록 충돌 목록을 채운다(PG 시점
    //   복구 경로와 같은 방식). --skip-precheck면 건너뛴다.
    if is_pg && !request.skip_precheck {
        let pg = crate::engine::postgres::conn::PgClient::connect(
            &request.target_uri,
            request.timeout_secs,
        )
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("복구 대상(PG) 연결 실패: {e}")))?;
        let existing = crate::engine::postgres::meta::list_qualified(pg.client())
            .await
            .map_err(|e| XBackupError::PrecheckFailed(format!("기존 테이블 조회 실패: {e}")))?;
        plan.conflicting_namespaces = existing;
    }
    if is_mysql && !request.skip_precheck {
        let mut my = crate::engine::mysql::conn::MysqlClient::connect(
            &request.target_uri,
            request.timeout_secs,
        )
        .await
        .map_err(|e| XBackupError::PrecheckFailed(format!("복구 대상(MySQL) 연결 실패: {e}")))?;
        let existing = crate::engine::mysql::meta::list_qualified(my.conn_mut())
            .await
            .map_err(|e| XBackupError::PrecheckFailed(format!("기존 테이블 조회 실패: {e}")))?;
        plan.conflicting_namespaces = existing;
    }

    // 버전 호환 경고는 진행 전 항상 알린다(차단하지 않음 — PRD: 경고).
    if let Some(warning) = &plan.version_warning {
        tracing::warn!("{warning}");
    }

    // dry-run: 계획만 반환하고 무변경 종료(exit 0). mongorestore 미스폰.
    if request.dry_run {
        tracing::info!(backup_id = %plan.backup_id, "dry-run — 복구 계획만 출력(무변경)");
        return Ok(RestoreOutcome {
            backup_id: plan.backup_id.clone(),
            stored_size_bytes: plan.stored_size_bytes,
            plan,
        });
    }

    // 가드레일: 기존 데이터(충돌)가 있고 --force가 없으면 —
    //   TTY면 대화형 확인, 비-TTY면 거부(exit 1).
    let drop_existing = decide_guard(&plan, request.force, is_tty, confirm)?;

    // 실제 복구 스트리밍(진행 카운터는 request.progress_counter에서 가져온다 — R16).
    stream_restore(request, storage, &plan, drop_existing).await?;

    tracing::info!(
        backup_id = %plan.backup_id,
        bytes = plan.stored_size_bytes,
        "풀 복구 완료"
    );
    Ok(RestoreOutcome {
        backup_id: plan.backup_id.clone(),
        stored_size_bytes: plan.stored_size_bytes,
        plan,
    })
}

/// `restore --to-dir` 결과 — 추출한 백업 ID·출력 경로·집계.
#[derive(Debug, Clone)]
pub struct ExportOutcome {
    /// 추출한 백업 ID.
    pub backup_id: String,
    /// 출력 디렉터리(mongodump 레이아웃 루트).
    pub out_dir: PathBuf,
    /// 컬렉션/문서/바이트 집계.
    pub summary: ExportSummary,
}

/// 백업을 **서버 없이** mongodump `--out` 레이아웃의 로컬 디렉터리로 추출한다.
///
/// 백업 선택(`--id` 우선, 없으면 최신 풀)·역스택(복호화→압축해제)은 복구 경로와 동일하게
/// 재사용하고, mongorestore 대신 [`native_export_to_dir`]로 `.bson`+`.metadata.json`을 쓴다.
/// **native(기본) 풀 백업만** 지원한다 — mongodump/PG 포맷·증분은 명확히 거부한다(추출 결과가
/// 표준 도구로 안 열리거나 단독 덤프가 아니기 때문). `only`는 ns 필터(`db.collection`).
pub async fn export_to_dir(
    storage: &dyn Storage,
    backup_id: Option<&str>,
    out_dir: &Path,
    only: Option<&str>,
) -> Result<ExportOutcome> {
    let store = ManifestStore::new(storage);
    let manifest = match backup_id {
        Some(id) => store.read(id).await?,
        None => latest_full_manifest(storage).await?,
    };

    // 가드 1: native 포맷만(mongodump archive/PG는 표준 BSON 덤프로 못 푼다).
    let fmt = manifest.tool_versions.archive_format.as_deref();
    if fmt != Some(crate::engine::native::archive::FORMAT_ID) {
        return Err(XBackupError::Usage(format!(
            "--to-dir는 native(기본) mongo 백업만 BSON 덤프로 추출합니다(이 백업 포맷: {}). \
             mongodump/PG 백업은 임시 서버로 복구한 뒤 표준 도구로 추출하세요.",
            fmt.unwrap_or("unknown")
        )));
    }
    // 가드 2: 풀 백업만(증분은 oplog 슬라이스라 단독 덤프 디렉터리가 아니다).
    if !matches!(manifest.backup_type, BackupType::Full) {
        return Err(XBackupError::Usage(
            "증분 백업은 --to-dir로 추출할 수 없습니다 — 풀 백업만 가능합니다(증분은 \
             서버 복구/PITR로 적용하세요)."
                .into(),
        ));
    }

    // data.bin → 역스택(복호화→압축해제) → 네이티브 프레임 스트림.
    let stages = reverse_stack_for(&manifest)?;
    let raw = storage.get_stream(&data_path(&manifest.id)).await?;
    let mut stream = stages.apply(raw);
    let summary = native_export_to_dir(&mut stream, out_dir, only).await?;

    Ok(ExportOutcome {
        backup_id: manifest.id,
        out_dir: out_dir.to_path_buf(),
        summary,
    })
}

/// 덮어쓰기 가드 결정 — drop을 켤지(true) 여부를 반환하거나 거부 에러를 낸다.
///
/// 규칙(PRD §FR-3 가드레일):
/// - 충돌(기존 데이터)이 없으면 force 값 그대로(보통 false) — drop 불필요.
/// - `--force`면 무조건 통과(drop 허용).
/// - 충돌이 있고 force 없음:
///   - TTY: `confirm` 콜백으로 대화형 확인. 승인하면 drop 허용, 거부하면 취소(exit 1).
///   - 비-TTY: 거부(exit 1) — 안전 기본값(자동화 환경에서 사고 방지).
fn decide_guard<C>(plan: &RestorePlan, force: bool, is_tty: bool, confirm: C) -> Result<bool>
where
    C: FnOnce(&RestorePlan) -> bool,
{
    if force {
        return Ok(true);
    }
    if !plan.has_conflicts() {
        // 빈 대상 — drop이 필요 없다(파괴적 옵션 미사용).
        return Ok(false);
    }
    if is_tty {
        if confirm(plan) {
            // 대화형 승인 = force 동등(drop 허용).
            Ok(true)
        } else {
            Err(XBackupError::Failure(
                "사용자가 복구를 취소했습니다(기존 데이터 보존)".into(),
            ))
        }
    } else {
        Err(XBackupError::Failure(format!(
            "복원 대상에 기존 데이터가 있습니다({}개 네임스페이스). 비-TTY에서는 \
             --force 없이 덮어쓰기를 거부합니다 — 충돌: {}",
            plan.conflicting_namespaces.len(),
            preview_namespaces(&plan.conflicting_namespaces)
        )))
    }
}

/// data.bin을 읽어 역스택을 통과시켜 mongorestore stdin으로 스트리밍한다.
///
/// `request.progress_counter`가 `Some`이면 stdin으로 흘리는 바이트를 누산해 진행
/// 표시기가 폴링한다(R16).
async fn stream_restore(
    request: &RestoreRequest,
    storage: &dyn Storage,
    plan: &RestorePlan,
    drop_existing: bool,
) -> Result<()> {
    // manifest를 다시 읽어 reverse 스택을 합성한다(평문이면 identity, 압축/암호화면 t6).
    let store = ManifestStore::new(storage);
    let manifest = store.read(&plan.backup_id).await?;
    let stages = reverse_stack_for(&manifest)?;

    // 1) storage에서 data.bin 읽기 스트림 확보 → 역스택 적용.
    let data_rel = data_path(&plan.backup_id);
    let raw = storage.get_stream(&data_rel).await?;
    let restored = stages.apply(raw);
    // 진행 카운터가 주입됐으면 복원 입력으로 흘리는 바이트를 누산한다(R16).
    // (역스택 출력 = 복원 입력 바이트. data.bin 저장 크기와 무관히 실제 통과량을 센다.)
    let mut restored_stream: BoxAsyncRead = match &request.progress_counter {
        Some(counter) => Box::pin(ProgressCountingReader::new(restored, Arc::clone(counter))),
        None => restored,
    };

    // 2) 엔진 분기 — manifest의 archive_format으로 백업을 만든 엔진을 식별한다.
    //    네이티브 포맷이면 드라이버로 직접 복원(외부 도구 불필요), PG면 COPY 복원, 그 외는 mongorestore.
    let archive_format = manifest.tool_versions.archive_format.as_deref();
    if archive_format == Some(crate::engine::native::archive::FORMAT_ID) {
        return native_stream_restore(request, &mut restored_stream, plan, drop_existing).await;
    }
    if archive_format == Some(crate::engine::postgres::archive::FORMAT_ID) {
        let inserted = crate::engine::postgres::restore::pg_restore(
            &mut restored_stream,
            &request.target_uri,
            request.timeout_secs,
            drop_existing,
        )
        .await?;
        tracing::debug!(backup_id = %plan.backup_id, inserted, "PG 복구: 행 삽입 완료");
        return Ok(());
    }
    if archive_format == Some(crate::engine::mysql::archive::FORMAT_ID) {
        let inserted = crate::engine::mysql::restore::mysql_restore(
            &mut restored_stream,
            &request.target_uri,
            request.timeout_secs,
            drop_existing,
        )
        .await?;
        tracing::debug!(backup_id = %plan.backup_id, inserted, "MySQL 복구: 행 삽입 완료");
        return Ok(());
    }

    // 3) (레거시 mongodump 아카이브 경로) legacy-mongodump feature 빌드에서만 지원한다 —
    //    기본 빌드는 서브프로세스 스폰 코드가 없다(로드맵 P0-2).
    #[cfg(not(feature = "legacy-mongodump"))]
    {
        Err(XBackupError::Failure(format!(
            "백업 '{}'는 mongodump 아카이브 포맷입니다 — 이 빌드에는 mongorestore \
             오케스트레이션이 포함되지 않았습니다(cargo feature `legacy-mongodump`). \
             legacy-mongodump feature를 켠 빌드로 복구하거나, native 엔진으로 새 백업을 \
             만드세요",
            plan.backup_id
        )))
    }
    #[cfg(feature = "legacy-mongodump")]
    {
        // URI를 0600 임시 config로(argv 노출 금지). 핸들은 restore 종료까지 유지.
        let uri_config = UriConfigFile::create(&request.target_uri)?;

        // 4) mongorestore 스폰(--archive=- stdin, --drop은 가드 통과 시에만).
        let spec = RestoreSpec {
            program: request.mongorestore_program.clone(),
            uri_config_path: uri_config.path().to_string(),
            ns_include: plan.ns_include.clone(),
            drop: drop_existing,
        };
        let mut restore = RestoreProcess::spawn(&spec)?;
        let mut stdin = restore.take_stdin()?;

        // 5) 파이프라인 바이트를 stdin으로 흘린다. 끝나면 stdin을 닫아 EOF를 보낸다.
        let copy_result = tokio::io::copy(&mut restored_stream, &mut stdin).await;
        // stdin을 명시적으로 닫는다(Drop이 닫지만 shutdown으로 flush 보장).
        use tokio::io::AsyncWriteExt;
        let _ = stdin.shutdown().await;
        drop(stdin);

        if let Err(copy_err) = copy_result {
            // 입력/파이프 실패: restore를 kill+wait로 정리(좀비 방지).
            restore.abort().await;
            return Err(XBackupError::Failure(format!(
                "복구 입력 스트리밍 실패: {copy_err}"
            )));
        }

        // 6) mongorestore 종료 코드 판정(exit code only).
        restore.wait().await
    }
}

/// 네이티브 아카이브를 드라이버로 직접 복원한다(외부 mongorestore 불필요).
///
/// [`native_restore`](crate::engine::native::restore::native_restore)에 역스택 출력 스트림을
/// 그대로 넘긴다 — createCollection(옵션)·createIndexes·insert_many로 복원한다.
async fn native_stream_restore(
    request: &RestoreRequest,
    reader: &mut BoxAsyncRead,
    plan: &RestorePlan,
    drop_existing: bool,
) -> Result<()> {
    let options =
        crate::engine::mongo::conn::client_options(&request.target_uri, request.timeout_secs)
            .await?;
    let client = mongodb::Client::with_options(options).map_err(|e| {
        XBackupError::Failure(format!("복구 대상 MongoDB 클라이언트 생성 실패: {e}"))
    })?;

    let inserted = crate::engine::native::restore::native_restore(
        reader,
        &client,
        drop_existing,
        plan.ns_include.as_deref(),
    )
    .await?;
    tracing::debug!(backup_id = %plan.backup_id, inserted, "네이티브 복구: 문서 삽입 완료");
    Ok(())
}

/// 저장된 모든 백업 중 **최신 풀 백업**의 manifest를 고른다.
///
/// 백업 ID는 UUID v7(시간 정렬 가능)이지만, 신뢰 가능한 기준은 manifest.created_at다.
/// 모든 `*/manifest.json`을 읽어 full + complete 중 created_at(동률 시 id) 최대를 택한다.
async fn latest_full_manifest(storage: &dyn Storage) -> Result<BackupManifest> {
    let store = ManifestStore::new(storage);
    let entries = storage.list("").await?;

    // manifest.json 경로에서 backup id를 추출한다(`<id>/manifest.json`).
    let mut ids: Vec<String> = entries
        .iter()
        .filter_map(|e| {
            let suffix = format!("/{MANIFEST_FILE}");
            e.path
                .strip_suffix(&suffix)
                .filter(|id| !id.is_empty() && !id.contains('/'))
                .map(|id| id.to_string())
        })
        .collect();
    ids.sort();
    ids.dedup();

    let mut best: Option<BackupManifest> = None;
    for id in ids {
        // 깨진/부분 manifest는 건너뛴다(읽기 실패는 무시하고 다음 후보로).
        let manifest = match store.read(&id).await {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!(id = %id, "manifest 읽기 실패(건너뜀): {e}");
                continue;
            }
        };
        // 풀 백업만 자동 선택 대상(증분은 base가 필요 — t9/t8 경로).
        if !matches!(manifest.backup_type, BackupType::Full) {
            continue;
        }
        let is_newer = match &best {
            None => true,
            Some(cur) => {
                (manifest.created_at.as_str(), manifest.id.as_str())
                    > (cur.created_at.as_str(), cur.id.as_str())
            }
        };
        if is_newer {
            best = Some(manifest);
        }
    }

    best.ok_or_else(|| {
        XBackupError::Failure(
            "복구할 풀 백업이 없습니다 — destination에서 완료된 풀 백업을 찾지 못했습니다".into(),
        )
    })
}

/// 대상 서버 버전과 manifest 기록 버전의 호환 경고를 만든다(차단 아님, PRD §FR-3).
///
/// 메이저.마이너가 다르면 경고한다 — 논리 dump/restore는 보통 호환되나, 메이저 다운그레이드
/// (예: 7.x → 6.x 복원)는 위험할 수 있어 운영자에게 알린다.
fn version_compat_warning(target: &ServerMeta, manifest: &BackupManifest) -> Option<String> {
    let src = major_minor(&manifest.server_version);
    let dst = major_minor(&target.server_version);
    match (src, dst) {
        (Some((s_maj, s_min)), Some((d_maj, d_min))) if (s_maj, s_min) != (d_maj, d_min) => {
            Some(format!(
                "서버 버전 불일치 경고 — 백업 원본 {}, 복구 대상 {}. 메이저/마이너가 \
                 다르면 복원이 실패하거나 호환성 문제가 생길 수 있습니다(특히 다운그레이드).",
                manifest.server_version, target.server_version
            ))
        }
        _ => None,
    }
}

/// `"7.0.35"` → `Some((7, 0))`. 파싱 실패 시 None(경고 생략).
fn major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse::<u32>().ok()?;
    let minor = parts.next().unwrap_or("0").parse::<u32>().ok()?;
    Some((major, minor))
}

/// 충돌 네임스페이스 미리보기(최대 5개 + 나머지 개수). 로그·에러 메시지용.
fn preview_namespaces(namespaces: &[String]) -> String {
    const MAX: usize = 5;
    if namespaces.len() <= MAX {
        return namespaces.join(", ");
    }
    let shown = namespaces[..MAX].join(", ");
    format!("{shown}, … (+{}개)", namespaces.len() - MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{BackupStatus, Topology, FORMAT_VERSION};
    use crate::storage::LocalFs;

    fn full_manifest(id: &str, created_at: &str, server_version: &str) -> BackupManifest {
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at: created_at.to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            topology: Topology::ReplicaSet,
            server_version: server_version.to_string(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 100,
            stored_size_bytes: 100,
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

    /// data.bin을 평문으로 저장하는 헬퍼(테스트용 백업 산출물 조립).
    async fn seed_backup(fs: &LocalFs, manifest: &BackupManifest, data: &[u8]) {
        use crate::storage::BoxAsyncRead;
        let reader: BoxAsyncRead = Box::pin(std::io::Cursor::new(data.to_vec()));
        fs.put_stream(&data_path(&manifest.id), reader, Some(data.len() as u64))
            .await
            .unwrap();
        ManifestStore::new(fs).write(manifest).await.unwrap();
    }

    /// 최신 풀 백업 자동 선택: created_at 최대를 고른다.
    #[tokio::test]
    async fn latest_full_picks_newest_by_created_at() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        seed_backup(
            &fs,
            &full_manifest("bk-old", "2026-06-10T00:00:00Z", "7.0.35"),
            b"old",
        )
        .await;
        seed_backup(
            &fs,
            &full_manifest("bk-new", "2026-06-12T00:00:00Z", "7.0.35"),
            b"new",
        )
        .await;

        let picked = latest_full_manifest(&fs).await.unwrap();
        assert_eq!(picked.id, "bk-new");
    }

    /// 증분 백업은 자동 선택 대상에서 제외된다(풀만).
    #[tokio::test]
    async fn latest_full_skips_incremental() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();

        let mut incr = full_manifest("bk-incr", "2026-06-13T00:00:00Z", "7.0.35");
        incr.backup_type = BackupType::Incremental;
        incr.base_id = Some("bk-full".into());
        seed_backup(&fs, &incr, b"incr").await;
        seed_backup(
            &fs,
            &full_manifest("bk-full", "2026-06-11T00:00:00Z", "7.0.35"),
            b"full",
        )
        .await;

        let picked = latest_full_manifest(&fs).await.unwrap();
        // 증분이 더 최신이지만 풀만 선택 → bk-full.
        assert_eq!(picked.id, "bk-full");
    }

    /// 풀 백업이 하나도 없으면 명확한 실패(exit 1).
    #[tokio::test]
    async fn latest_full_errors_when_none() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let err = latest_full_manifest(&fs).await.unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    /// dry-run은 mongorestore를 스폰하지 않고 계획만 반환한다(무부작용, exit 0).
    /// skip_precheck=true로 대상 연결 없이 계획을 만든다.
    #[tokio::test]
    async fn dry_run_produces_plan_without_side_effects() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        seed_backup(
            &fs,
            &full_manifest("bk-dry", "2026-06-12T00:00:00Z", "7.0.35"),
            b"payload",
        )
        .await;

        let request = RestoreRequest {
            target_uri: crate::config::secret::Secret::new("mongodb://unused/db"),
            // dry-run + skip-precheck면 이 프로그램은 절대 실행되지 않아야 한다.
            mongorestore_program: "/nonexistent/should-never-run".to_string(),
            backup_id: Some("bk-dry".to_string()),
            only: None,
            force: false,
            dry_run: true,
            skip_precheck: true,
            timeout_secs: None,
            progress_counter: None,
        };

        // confirm은 호출되지 않아야 한다(dry-run).
        let outcome = run_restore(&request, &fs, false, |_| panic!("confirm 호출되면 안 됨"))
            .await
            .unwrap();

        assert_eq!(outcome.backup_id, "bk-dry");
        // dry-run 계획은 manifest의 stored_size_bytes(예상 크기)를 보고한다 — 실제
        // data.bin 바이트 수가 아니다(실측 다운로드 없이 계획만 산출).
        assert_eq!(outcome.stored_size_bytes, 100);
        assert!(!outcome.plan.has_conflicts());
    }

    /// H6 — PG 대상 복구에서 `--only`(선택적 복구)는 Usage(exit 2)로 거부된다. 거부는 백업
    /// 선택·대상 연결보다 먼저 일어나므로 DB 없이 검증된다.
    #[tokio::test]
    async fn pg_restore_rejects_only_selective() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let request = RestoreRequest {
            target_uri: crate::config::secret::Secret::new("postgres://unused/db"),
            mongorestore_program: "/nonexistent/should-never-run".to_string(),
            backup_id: Some("bk".to_string()),
            only: Some("public.orders".to_string()),
            force: false,
            dry_run: true,
            skip_precheck: true,
            timeout_secs: None,
            progress_counter: None,
        };
        let err = run_restore(&request, &fs, false, |_| {
            panic!("거부 전이라 confirm 미호출")
        })
        .await
        .expect_err("PG --only는 거부되어야 함");
        assert_eq!(
            err.exit_code(),
            2,
            "PG --only는 Usage(exit 2)여야 함: {err}"
        );
    }

    fn plan_with_conflicts(conflicts: Vec<String>) -> RestorePlan {
        RestorePlan {
            backup_id: "bk".into(),
            backup_type: BackupType::Full,
            source_server_version: "7.0.35".into(),
            stored_size_bytes: 100,
            ns_include: None,
            conflicting_namespaces: conflicts,
            target_server_version: Some("7.0.35".into()),
            version_warning: None,
        }
    }

    /// 가드: 충돌 있고 비-TTY + --force 없음 → 거부(exit 1).
    #[test]
    fn guard_rejects_existing_data_without_force_non_tty() {
        let plan = plan_with_conflicts(vec!["testdb.items".into()]);
        let err = decide_guard(&plan, false, false, |_| true).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("기존 데이터"), "메시지: {err}");
    }

    /// 가드: --force면 충돌이 있어도 통과하고 drop을 켠다(true).
    #[test]
    fn guard_force_allows_drop() {
        let plan = plan_with_conflicts(vec!["testdb.items".into()]);
        let drop = decide_guard(&plan, true, false, |_| panic!("force면 confirm 미호출")).unwrap();
        assert!(drop, "force는 drop을 허용해야 함");
    }

    /// 가드: 충돌 없으면 force 없이도 통과하되 drop은 끈다(false).
    #[test]
    fn guard_no_conflicts_passes_without_drop() {
        let plan = plan_with_conflicts(vec![]);
        let drop = decide_guard(&plan, false, false, |_| {
            panic!("충돌 없으면 confirm 미호출")
        })
        .unwrap();
        assert!(!drop, "충돌 없으면 drop 불필요");
    }

    /// 가드: TTY에서 대화형 확인 승인 → 통과(drop 허용), 거부 → 취소(exit 1).
    #[test]
    fn guard_tty_interactive_confirm() {
        let plan = plan_with_conflicts(vec!["testdb.items".into()]);
        // 승인.
        let drop = decide_guard(&plan, false, true, |_| true).unwrap();
        assert!(drop);
        // 거부.
        let err = decide_guard(&plan, false, true, |_| false).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("취소"), "메시지: {err}");
    }

    #[test]
    fn preview_namespaces_truncates() {
        let many: Vec<String> = (0..8).map(|i| format!("db.c{i}")).collect();
        let preview = preview_namespaces(&many);
        assert!(preview.contains("(+3개)"), "preview: {preview}");
        let few = vec!["db.a".to_string(), "db.b".to_string()];
        assert_eq!(preview_namespaces(&few), "db.a, db.b");
    }

    /// 버전 호환 경고: 메이저/마이너가 다르면 경고, 같으면 None.
    #[test]
    fn version_warning_on_major_minor_mismatch() {
        let manifest = full_manifest("x", "t", "7.0.35");
        let same = ServerMeta {
            repl_set_name: Some("rs0".into()),
            server_version: "7.0.99".into(),
        };
        assert!(
            version_compat_warning(&same, &manifest).is_none(),
            "패치 차이는 경고 없음"
        );

        let downgrade = ServerMeta {
            repl_set_name: Some("rs0".into()),
            server_version: "6.0.10".into(),
        };
        let w = version_compat_warning(&downgrade, &manifest).expect("메이저 차이 경고");
        assert!(w.contains("7.0.35") && w.contains("6.0.10"), "경고: {w}");
    }

    #[test]
    fn major_minor_parses() {
        assert_eq!(major_minor("7.0.35"), Some((7, 0)));
        assert_eq!(major_minor("6.2"), Some((6, 2)));
        assert_eq!(major_minor("8"), Some((8, 0)));
        assert_eq!(major_minor("garbage"), None);
    }

    /// 가짜 mongorestore 스크립트 — stdin을 sink 파일로 복사하고 exit 0.
    #[cfg(feature = "legacy-mongodump")]
    fn fake_mongorestore_sink(sink_path: &str) -> tempfile::TempPath {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        let mut f = tempfile::Builder::new()
            .prefix("fake-mongorestore-pipe-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        writeln!(
            f,
            "#!/usr/bin/env bash\n>&2 echo 'restoring...'\ncat > {sink:?}\nexit 0",
            sink = sink_path
        )
        .unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();
        path
    }

    /// 평문 풀 복구 스트리밍 E2E(가짜 mongorestore): storage data.bin → identity 역스택
    /// → mongorestore stdin으로 바이트가 손실 없이 전달되는지 확인한다. DB·실제 도구
    /// 없이 파이프라인 합성·스폰·stdin copy·exit 판정 전 구간을 검증한다.
    #[cfg(feature = "legacy-mongodump")]
    #[tokio::test]
    async fn plaintext_restore_streams_data_to_mongorestore_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        let payload = b"ARCHIVE_BYTES_FOR_RESTORE_streamed_via_stdin";
        seed_backup(
            &fs,
            &full_manifest("bk-pipe", "2026-06-12T00:00:00Z", "7.0.35"),
            payload,
        )
        .await;

        let sink = tempfile::NamedTempFile::new().unwrap();
        let sink_path = sink.path().to_str().unwrap().to_string();
        let fake = fake_mongorestore_sink(&sink_path);

        let request = RestoreRequest {
            target_uri: crate::config::secret::Secret::new("mongodb://unused/db"),
            mongorestore_program: fake.to_str().unwrap().to_string(),
            backup_id: Some("bk-pipe".to_string()),
            only: None,
            force: true, // 가드 통과(drop 허용).
            dry_run: false,
            skip_precheck: true, // DB 연결 없이 스트리밍 경로만 검증.
            timeout_secs: None,
            progress_counter: None,
        };

        let outcome = run_restore(&request, &fs, false, |_| panic!("force면 confirm 미호출"))
            .await
            .expect("평문 복구 스트리밍 성공");
        assert_eq!(outcome.backup_id, "bk-pipe");

        // mongorestore stdin이 받은 바이트가 원본 data.bin과 동일해야 한다(손실 없음).
        let received = std::fs::read(&sink_path).unwrap();
        assert_eq!(
            received, payload,
            "stdin으로 전달된 바이트가 data.bin과 불일치"
        );
    }

    /// 미포함(기본) 빌드: mongodump 아카이브 포맷 백업의 복구는 명확한 안내와 함께
    /// 실패(exit 1)한다 — legacy-mongodump 빌드 또는 native 재백업 유도(P0-2).
    #[cfg(not(feature = "legacy-mongodump"))]
    #[tokio::test]
    async fn restore_rejects_mongodump_format_without_legacy_feature() {
        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        seed_backup(
            &fs,
            &full_manifest("bk-legacy", "2026-06-12T00:00:00Z", "7.0.35"),
            b"x",
        )
        .await;

        let request = RestoreRequest {
            target_uri: crate::config::secret::Secret::new("mongodb://unused/db"),
            mongorestore_program: "/nonexistent/never-spawned".to_string(),
            backup_id: Some("bk-legacy".to_string()),
            only: None,
            force: true,
            dry_run: false,
            skip_precheck: true,
            timeout_secs: None,
            progress_counter: None,
        };
        let err = run_restore(&request, &fs, false, |_| true)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains("legacy-mongodump"),
            "feature 안내 누락: {err}"
        );
    }

    /// mongorestore가 비정상 종료(exit 1)하면 복구는 실패(exit 1)로 전파한다.
    #[cfg(feature = "legacy-mongodump")]
    #[tokio::test]
    async fn restore_propagates_mongorestore_failure() {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let fs = LocalFs::new(dir.path()).unwrap();
        seed_backup(
            &fs,
            &full_manifest("bk-fail", "2026-06-12T00:00:00Z", "7.0.35"),
            b"x",
        )
        .await;

        // exit 1로 끝나는 가짜 mongorestore.
        let mut f = tempfile::Builder::new()
            .prefix("fake-mongorestore-fail-")
            .suffix(".sh")
            .tempfile()
            .unwrap();
        writeln!(
            f,
            "#!/usr/bin/env bash\ncat > /dev/null\n>&2 echo 'Failed: boom'\nexit 1"
        )
        .unwrap();
        let path = f.into_temp_path();
        let mut perms = std::fs::metadata(&path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).unwrap();

        let request = RestoreRequest {
            target_uri: crate::config::secret::Secret::new("mongodb://unused/db"),
            mongorestore_program: path.to_str().unwrap().to_string(),
            backup_id: Some("bk-fail".to_string()),
            only: None,
            force: true,
            dry_run: false,
            skip_precheck: true,
            timeout_secs: None,
            progress_counter: None,
        };
        let err = run_restore(&request, &fs, false, |_| true)
            .await
            .unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }
}
