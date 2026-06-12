//! PITR(Point-In-Time Recovery) — base 풀 복원 + 증분 oplog 슬라이스 재생(PRD §6.4, FR-3).
//!
//! 풀 복구([`super::restore`])가 dump 시점 스냅샷만 복원하는 데 비해, PITR은 그 위에
//! base 이후의 증분 oplog 슬라이스들을 **체인 순서로 재생**해 `--at <RFC3339>` 시점으로
//! 복구한다. chain 검증([`crate::manifest::chain::verify_chain`]) 통과가 전제다.
//!
//! ## 흐름(PRD §6.4)
//! ```text
//! --at(RFC3339 UTC) → target_unix
//!   → 체인 수집·verify_chain(연속성 거부 시 exit 1, "verify --chain" 안내)
//!   → base 풀 복원(t5 run_restore 재사용; --at 직전 base를 backup_id로 고정)
//!   → 증분 슬라이스를 oplog_range 순서로:
//!        복호화·해제(reverse_stack_for) → temp oplog.bson(0600) 배치 → mongorestore 재생
//!        마지막(목표 시점이 걸친) 슬라이스에만 --oplogLimit 적용
//!   → 결정된 종료 ts({t,i}) + wall-clock 보고
//! ```
//!
//! ## oplog 재생 경로(실측 채택 — 스파이크 본 태스크)
//! docs/spike-oplog-archive.md는 풀 archive의 `--oplogReplay`만 실측했고 **증분 슬라이스
//! 재생은 미실측**이었다. 본 태스크에서 mongo:7 + database-tools 100.x로 두 후보를 실측:
//! - **① 빈 dump 디렉터리 + `oplog.bson` 배치 + `mongorestore --dir <dir> --oplogReplay`** — ✅ 채택
//! - ② `--oplogFile <path> --oplogReplay`(+빈 `--dir`) — 동작은 하나 디렉터리 레이아웃이
//!   덜 표준적이라 미채택.
//!
//! 증분 산출물은 raw oplog BSON 연결 스트림(복호화·해제 후)이며, 이는 mongodump가
//! `local.oplog.rs`를 dump한 `oplog.bson`과 **바이트 동형**(연결된 BSON 문서들)이라 ①에
//! 그대로 배치할 수 있음을 확인했다.
//!
//! ## --oplogLimit 보정 규칙(실측)
//! `mongorestore --oplogLimit <seconds>[:<ordinal>]`은 **미만(<) 의미**다 — 한계 ts와
//! 같거나 큰 엔트리는 적용하지 않는다. FR-3는 "이하(<=)" 복구를 요구하므로, 결정한 종료
//! ts `{t,i}`를 **그대로** 포함하려면 한계를 `{t, i+1}`로 전달한다(`+1` 보정,
//! [`oplog_limit_arg`]). 실측 확인:
//! - limit `t:i`(보정 없음) → 해당 엔트리 **제외**.
//! - limit `t:(i+1)`(보정) → 해당 엔트리 **포함**, 그 다음은 제외.
//!
//! ## 디스크 임시 저장(스트리밍 원칙의 예외 — 재생 단계 한정)
//! 전 구간 스트리밍이 원칙이나(PRD §7), `mongorestore --oplogReplay`는 `oplog.bson`
//! **파일**을 기대하므로 슬라이스를 복호화·해제한 평문을 임시 파일(0600)에 잠깐 쓴 뒤
//! 재생 후 즉시 삭제한다([`TempDir`]가 Drop 시 정리). oplog 슬라이스는 풀 dump 대비
//! 작아 허용 가능한 예외다(태스크 지침).

use std::path::Path;
use std::process::Stdio;

use chrono::{DateTime, Utc};
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::config::secret::Secret;
use crate::engine::mongo::UriConfigFile;
use crate::error::{Result, XBackupError};
use crate::manifest::chain::{verify_chain, ChainNode, ChainReport};
use crate::manifest::schema::{BackupManifest, BackupType, OplogTimestamp};
use crate::manifest::store::{data_path, ManifestStore};
use crate::pipeline::restore::{run_restore, RestoreRequest};
use crate::pipeline::stage::reverse_stack_for;
use crate::storage::Storage;

/// PITR 복구 요청.
pub struct PitrRequest {
    /// 복구 대상 MongoDB URI 시크릿(`--target` 우선, 없으면 프로파일 source).
    pub target_uri: Secret,
    /// mongorestore 실행파일 경로(보통 `"mongorestore"`).
    pub mongorestore_program: String,
    /// PITR 목표 시점(RFC3339 UTC wall-clock, `--at`).
    pub at: String,
    /// 기존 데이터 덮어쓰기 가드 해제(`--force`).
    pub force: bool,
    /// 복구 계획만 출력(`--dry-run`) — 무변경.
    pub dry_run: bool,
    /// 복구 사전 점검 우회(`--skip-precheck`).
    pub skip_precheck: bool,
}

/// PITR 복구 계획(dry-run 출력·실행 요약 공통). 시크릿은 담지 않는다.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PitrPlan {
    /// base 풀백업 ID.
    pub base_id: String,
    /// 재생할 증분 슬라이스(체인 순서) — base 이후 종료 시점까지.
    pub incremental_ids: Vec<String>,
    /// 파싱된 목표 unix 초(`--at` → UTC epoch). 보고·진단용.
    pub target_unix: i64,
    /// 내림 매핑으로 결정한 종료 oplog ts(`ts.t <= target_unix` 중 최대). 빈 체인이면 base end.
    pub decided_ts: OplogTimestamp,
    /// 결정 ts의 wall-clock(UTC RFC3339, 초 단위) — 사람이 읽는 보고용.
    pub decided_wall_clock: String,
    /// 재생을 종료시킬 마지막 슬라이스 ID(`decided_ts`를 포함하는 슬라이스). 없으면 None
    /// (base만으로 충분 — base end가 이미 목표 이상이거나 재생할 슬라이스 없음).
    pub limit_slice_id: Option<String>,
    /// 예상 복원 입력 크기(base + 재생 슬라이스들의 저장 바이트 합).
    pub estimated_bytes: u64,
}

/// PITR 복구 결과 요약(CLI 출력용).
#[derive(Debug, Clone)]
pub struct PitrOutcome {
    /// 수립된 복구 계획.
    pub plan: PitrPlan,
    /// 실제 재생한 슬라이스 수(빈 슬라이스 제외).
    pub replayed_slices: u64,
}

/// PITR 복구를 끝까지 실행한다(매핑 → 체인 검증 → base 복원 → 슬라이스 재생).
///
/// `is_tty`/`confirm`은 base 복원 가드레일용(t5 [`run_restore`]에 위임).
pub async fn run_pitr<C>(
    request: &PitrRequest,
    storage: &dyn Storage,
    is_tty: bool,
    confirm: C,
) -> Result<PitrOutcome>
where
    C: FnOnce(&crate::pipeline::restore::RestorePlan) -> bool,
{
    // 1) --at 파싱(RFC3339 UTC) → target_unix.
    let target = parse_at(&request.at)?;
    let target_unix = target.timestamp();

    // 2) 체인 수집 + base 식별(--at 직전의 base 풀백업) + 연속성 검증.
    let store = ManifestStore::new(storage);
    let nodes = collect_chain_nodes(storage).await?;

    let base_id = select_base_before(&nodes, target_unix)?;
    let report = verify_chain(&nodes, &base_id);
    if !report.is_continuous() {
        return Err(chain_rejection_error(&report));
    }

    // 3) 종료 ts 내림 매핑 + 마지막(limit) 슬라이스 결정.
    let base_node = find_node(&nodes, &report.base_id)
        .ok_or_else(|| XBackupError::Failure(format!("base '{}' 노드 소실", report.base_id)))?;
    let slices = ordered_slices(&nodes, &report.incremental_ids);
    let mapping = map_target_to_end(base_node, &slices, target_unix);

    // 4) 예상 크기 + 계획 조립(시크릿 미포함).
    let estimated_bytes =
        estimate_bytes(&store, &report.base_id, &mapping.replay_ids).await;
    let plan = PitrPlan {
        base_id: report.base_id.clone(),
        incremental_ids: mapping.replay_ids.clone(),
        target_unix,
        decided_ts: mapping.decided_ts,
        decided_wall_clock: wall_clock_of(mapping.decided_ts),
        limit_slice_id: mapping.limit_slice_id.clone(),
        estimated_bytes,
    };

    // 5) dry-run: 계획만 반환(무변경, exit 0).
    if request.dry_run {
        tracing::info!(
            base_id = %plan.base_id,
            decided = %plan.decided_wall_clock,
            slices = plan.incremental_ids.len(),
            "PITR dry-run — 복구 계획만 출력(무변경)"
        );
        return Ok(PitrOutcome {
            plan,
            replayed_slices: 0,
        });
    }

    // 6) base 풀 복원(t5 재사용). PITR base는 항상 풀백업·전체 복원이다.
    let base_request = RestoreRequest {
        target_uri: request.target_uri.clone(),
        mongorestore_program: request.mongorestore_program.clone(),
        backup_id: Some(report.base_id.clone()),
        only: None, // PITR은 --only 병용 불가(상위 핸들러가 거부; 여기선 항상 None).
        force: request.force,
        dry_run: false,
        skip_precheck: request.skip_precheck,
        // base 풀 복원은 PITR 자체 진행 표시(상위)에 위임 — 별도 카운터 미주입.
        progress_counter: None,
    };
    run_restore(&base_request, storage, is_tty, confirm).await?;

    // 7) 증분 슬라이스 재생(체인 순서). 마지막(limit) 슬라이스에만 --oplogLimit 적용.
    let replayed = replay_slices(request, storage, &store, &mapping).await?;

    tracing::info!(
        base_id = %plan.base_id,
        decided = %plan.decided_wall_clock,
        replayed = replayed,
        "PITR 복구 완료"
    );
    Ok(PitrOutcome {
        plan,
        replayed_slices: replayed,
    })
}

/// PITR + `--only`(선택적 복구) 병용을 거부한다(pitfall 1-4, PRD Edge Case).
///
/// `mongorestore --oplogReplay`는 `--nsInclude`(네임스페이스 필터)와 병용할 수 없다.
/// 따라서 PITR(oplog 재생)과 `--only`를 함께 지정하면 즉시 거부한다(exit 2). 핸들러가
/// 어떤 작업도 시작하기 전에 호출한다.
pub fn reject_pitr_with_only(only: Option<&str>) -> Result<()> {
    if only.is_some() {
        return Err(XBackupError::Usage(
            "PITR(--at)은 --only(선택적 복구)와 함께 쓸 수 없습니다 — oplog 재생(--oplogReplay)은 \
             네임스페이스 필터(--nsInclude)와 병용 불가합니다(전체 복구만 가능)"
                .into(),
        ));
    }
    Ok(())
}

/// `--at` RFC3339 UTC 문자열을 파싱한다. 잘못된 형식은 사용법 오류(exit 2).
fn parse_at(at: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(at)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|e| {
            XBackupError::Usage(format!(
                "--at 시각 파싱 실패('{at}'): {e} — RFC3339 형식이 필요합니다(예: 2026-06-12T13:00:00Z)"
            ))
        })
}

/// destination의 모든 manifest를 모아 체인 노드로 만든다(읽기 실패는 제외·디버그 로그).
async fn collect_chain_nodes(storage: &dyn Storage) -> Result<Vec<ChainNode>> {
    let ids = crate::pipeline::verify::collect_manifest_ids(storage).await?;
    let store = ManifestStore::new(storage);
    let mut nodes = Vec::with_capacity(ids.len());
    for id in &ids {
        match store.read(id).await {
            Ok(m) => nodes.push(ChainNode::from_manifest(&m)),
            Err(e) => tracing::debug!(id = %id, "manifest 읽기 실패(체인 노드 제외): {e}"),
        }
    }
    Ok(nodes)
}

/// `--at` 시각 **직전**의 base 풀백업 ID를 고른다.
///
/// 규칙(PRD §6.4 1단계): 목표 시점 이하에서 시작하는 가장 최신 풀백업을 base로 삼는다.
/// base의 기준점은 `oplog_range.end_ts`(dump 완료 직후 ts) — 그 t가 `target_unix` 이하여야
/// 그 위에 증분을 이어 목표 시점에 도달할 수 있다. 적격 base가 없으면 거부(exit 1).
///
/// 동률(같은 end ts)은 id 사전순(UUID v7 = 시간순) 최대로 깬다.
fn select_base_before(nodes: &[ChainNode], target_unix: i64) -> Result<String> {
    let mut best: Option<&ChainNode> = None;
    for n in nodes {
        if !matches!(n.backup_type, BackupType::Full) || n.selective {
            continue;
        }
        // base의 end_ts.t가 목표 이하인 풀백업만 후보(목표 이전에 찍힌 스냅샷).
        let end = match n.oplog_range {
            Some((_, e)) => e,
            None => continue, // standalone 풀백업은 oplog 기준점이 없어 PITR base 부적격.
        };
        if (end.t as i64) > target_unix {
            continue;
        }
        let is_better = match best {
            None => true,
            Some(cur) => {
                let cur_end = cur.oplog_range.map(|(_, e)| e).unwrap_or(end);
                (end, n.id.as_str()) > (cur_end, cur.id.as_str())
            }
        };
        if is_better {
            best = Some(n);
        }
    }
    best.map(|n| n.id.clone()).ok_or_else(|| {
        XBackupError::Failure(format!(
            "목표 시점({}) 이전의 base 풀백업을 찾지 못했습니다 — PITR에는 목표 시각보다 \
             앞선 풀백업이 필요합니다",
            wall_clock_of_unix(target_unix)
        ))
    })
}

/// 체인 검증 실패 → PITR 거부 에러(exit 1). 끊김 지점·안내를 메시지에 담는다.
fn chain_rejection_error(report: &ChainReport) -> XBackupError {
    let breaks = report
        .breaks
        .iter()
        .map(|b| b.to_string())
        .collect::<Vec<_>>()
        .join("; ");
    XBackupError::Failure(format!(
        "증분 체인이 연속적이지 않아 PITR을 거부합니다(base '{}'): {breaks} \
         — `x-backup verify --id <id> --chain`으로 체인 무결성을 확인하세요",
        report.base_id
    ))
}

/// `incremental_ids`(verify_chain이 정렬한 순서)에 대응하는 노드를 그 순서로 모은다.
fn ordered_slices<'a>(nodes: &'a [ChainNode], incremental_ids: &[String]) -> Vec<&'a ChainNode> {
    incremental_ids
        .iter()
        .filter_map(|id| find_node(nodes, id))
        .collect()
}

/// 종료 ts 내림 매핑 결과.
struct EndMapping {
    /// 결정된 종료 oplog ts(보고용; `ts.t <= target_unix` 중 최대 — 빈 체인이면 base end).
    /// 슬라이스 전체가 목표 이하면 그 슬라이스 end의 정확한 ts, 목표가 슬라이스를 걸치면
    /// `{target_unix, 0}`(목표 초 — 그 초의 모든 변경이 포함됨을 의미).
    decided_ts: OplogTimestamp,
    /// 실제 재생할 슬라이스 ID(종료 ts를 포함하는 마지막 슬라이스까지).
    replay_ids: Vec<String>,
    /// --oplogLimit를 적용할 마지막 슬라이스(종료 ts가 그 안에 걸침). None이면 한계 없이
    /// 전부 재생(목표가 마지막 슬라이스 end 이상).
    limit_slice_id: Option<String>,
    /// limit 슬라이스에 줄 `--oplogLimit <seconds>[:<ordinal>]` 인자(미만 의미, 이미 보정됨).
    /// `limit_slice_id`가 None이면 None(한계 없이 전부 재생).
    oplog_limit: Option<String>,
}

/// 목표 unix 초를 종료 ts로 내림 매핑하고 재생할 슬라이스·limit 슬라이스·oplogLimit을
/// 결정한다.
///
/// 슬라이스 메타(`oplog_range`)로 근사한다(엔트리 단위 검사 없이 oplogLimit에 위임).
/// **FR-3 "이하(<=)" 매핑**: `ts.t <= target_unix`인 oplog 엔트리는 i와 무관하게 모두
/// 포함해야 한다(목표 *초* 안의 모든 변경 포함 — 실측: writeA가 base와 같은 초여도 i로
/// 분리되어 포함돼야 함, t9 스파이크).
/// - 슬라이스 `end_ts.t <= target_unix`면 그 슬라이스는 **전부** 재생(한계 불필요).
/// - 슬라이스 `start_ts.t <= target_unix < end_ts.t`면 그 슬라이스가 목표를 **걸침** —
///   여기까지 재생하고 oplogLimit `{target_unix+1, 0}`(미만 의미 → 목표 초 전체 포함,
///   다음 초 제외)을 건다. 보고용 종료 ts는 `{target_unix, 0}`(목표 초).
/// - 모든 슬라이스 end가 목표 이하면 전부 재생(한계 없음), 종료 ts = 마지막 슬라이스 end.
fn map_target_to_end(
    base: &ChainNode,
    slices: &[&ChainNode],
    target_unix: i64,
) -> EndMapping {
    let base_end = base.oplog_range.map(|(_, e)| e).unwrap_or(OplogTimestamp::new(0, 0));

    let mut replay_ids = Vec::new();
    let mut decided_ts = base_end;
    let mut limit_slice_id = None;
    let mut oplog_limit = None;

    for slice in slices {
        // 빈 슬라이스(변경 없음)는 재생 대상이 아니다 — 체인 연속성 마커일 뿐(지침 5).
        let is_empty = slice.is_empty_slice;
        let (start, end) = match slice.oplog_range {
            Some(r) => r,
            None => continue, // 연속성 검증을 통과했다면 도달 불가(방어적).
        };

        if (end.t as i64) <= target_unix {
            // 슬라이스 전체가 목표 이하 — 전부 재생, 종료 ts를 이 슬라이스 end로 갱신.
            if !is_empty {
                replay_ids.push(slice.id.clone());
            }
            decided_ts = end;
            continue;
        }

        if (start.t as i64) <= target_unix {
            // 목표가 이 슬라이스를 걸침 — 여기까지 재생하고 한계를 건다.
            if !is_empty {
                replay_ids.push(slice.id.clone());
                limit_slice_id = Some(slice.id.clone());
                // 목표 초 전체 포함: oplogLimit = {target_unix+1, 0}(미만 의미).
                oplog_limit = Some(oplog_limit_for_second(target_unix as u32));
            }
            decided_ts = OplogTimestamp::new(target_unix as u32, 0); // 보고용: 목표 초.
        }
        // 목표가 이 슬라이스 start보다 앞서면(start.t > target) 이후 슬라이스는 모두 미래 —
        // 재생하지 않고 종료(slices는 ts 오름차순).
        break;
    }

    EndMapping {
        decided_ts,
        replay_ids,
        limit_slice_id,
        oplog_limit,
    }
}

/// 목표 *초*(`target_secs`)의 모든 변경을 포함하는 `--oplogLimit` 인자를 만든다.
///
/// 실측(t9 스파이크): `mongorestore --oplogLimit <seconds>[:<ordinal>]`은 **미만(<)
/// 의미**다. `ts.t <= target_secs`(이하)인 엔트리를 모두 포함하려면 한계를 `{target_secs+1,
/// 0}`으로 준다 — 목표 초 안의 임의 i를 모두 포함하고 다음 초 첫 엔트리부터 제외한다.
/// `target_secs`가 u32::MAX면(이론상) 오버플로 없이 한계를 그대로 둔다(saturating).
fn oplog_limit_for_second(target_secs: u32) -> String {
    let next = target_secs.saturating_add(1);
    format!("{next}:0")
}

/// 증분 슬라이스를 체인 순서로 재생한다. 마지막(limit) 슬라이스에만 --oplogLimit 적용.
async fn replay_slices(
    request: &PitrRequest,
    storage: &dyn Storage,
    store: &ManifestStore<'_>,
    mapping: &EndMapping,
) -> Result<u64> {
    let mut replayed = 0u64;
    for id in &mapping.replay_ids {
        let manifest = store.read(id).await?;
        let is_limit = mapping.limit_slice_id.as_deref() == Some(id.as_str());
        let limit = if is_limit {
            mapping.oplog_limit.as_deref()
        } else {
            None
        };
        replay_one_slice(request, storage, &manifest, limit).await?;
        replayed += 1;
    }
    Ok(replayed)
}

/// 단일 슬라이스를 재생한다 — 복호화·해제 → temp `oplog.bson`(0600) 배치 → mongorestore.
///
/// 재생 경로 ①(실측 채택): 빈 dump 디렉터리에 `oplog.bson`을 두고
/// `mongorestore --dir <dir> --oplogReplay [--oplogLimit <t>:<i>]`로 적용한다.
/// 슬라이스 평문을 임시 파일에 잠깐 쓰는 것은 스트리밍 원칙의 재생-단계 예외다(모듈 주석).
async fn replay_one_slice(
    request: &PitrRequest,
    storage: &dyn Storage,
    manifest: &BackupManifest,
    oplog_limit: Option<&str>,
) -> Result<()> {
    // 1) storage data.bin → reverse 스택(복호화→해제; 평문은 identity) → 디코드 스트림.
    let stages = reverse_stack_for(manifest)?;
    let raw = storage.get_stream(&data_path(&manifest.id)).await?;
    let mut decoded = stages.apply(raw);

    // 2) 임시 dump 디렉터리 + oplog.bson(0600)에 디코드 결과를 쓴다(재생 후 자동 삭제).
    let temp_dir = tempfile::Builder::new()
        .prefix("xb-pitr-replay-")
        .tempdir()
        .map_err(|e| XBackupError::Failure(format!("PITR 임시 디렉터리 생성 실패: {e}")))?;
    let oplog_path = temp_dir.path().join("oplog.bson");
    write_oplog_bson(&oplog_path, &mut decoded).await?;

    // 3) mongorestore 재생 스폰(URI는 0600 config, argv 노출 금지).
    let uri_config = UriConfigFile::create(&request.target_uri)?;
    spawn_replay(
        &request.mongorestore_program,
        temp_dir.path(),
        uri_config.path(),
        oplog_limit,
    )
    .await?;

    tracing::info!(
        slice = %manifest.id,
        limit = oplog_limit.unwrap_or("(없음)"),
        "PITR 슬라이스 재생 완료"
    );
    // temp_dir Drop → oplog.bson 삭제(사용 후 즉시 정리).
    Ok(())
}

/// 디코드 스트림을 0600 `oplog.bson` 파일로 쓴다(스트리밍 복사, 고정 버퍼).
async fn write_oplog_bson(path: &Path, decoded: &mut crate::storage::BoxAsyncRead) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let file = tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .await
        .map_err(|e| XBackupError::Failure(format!("oplog.bson 생성 실패: {e}")))?;
    let mut writer = tokio::io::BufWriter::new(file);

    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = decoded
            .read(&mut buf)
            .await
            .map_err(|e| XBackupError::Failure(format!("슬라이스 디코드 읽기 실패: {e}")))?;
        if n == 0 {
            break;
        }
        writer
            .write_all(&buf[..n])
            .await
            .map_err(|e| XBackupError::Failure(format!("oplog.bson 쓰기 실패: {e}")))?;
    }
    writer
        .flush()
        .await
        .map_err(|e| XBackupError::Failure(format!("oplog.bson flush 실패: {e}")))?;
    Ok(())
}

/// `mongorestore --dir <dir> --oplogReplay [--oplogLimit]`를 스폰해 재생한다.
///
/// 풀 복구(engine::mongo::restore)와 동일한 안전 규칙: URI는 `--config`(argv 금지),
/// stderr 독립 drain, exit code로만 판정, kill_on_drop.
///
/// **PITR + ns 필터 병용 금지(pitfall 1-4):** `--oplogReplay`는 `--nsInclude`와 병용
/// 불가하므로 여기서는 ns 필터를 절대 부여하지 않는다(상위 핸들러가 --only를 거부).
async fn spawn_replay(
    program: &str,
    dir: &Path,
    uri_config_path: &str,
    oplog_limit: Option<&str>,
) -> Result<()> {
    let mut cmd = Command::new(program);
    cmd.arg("--dir")
        .arg(dir)
        .arg("--oplogReplay")
        .arg("--config")
        .arg(uri_config_path);
    if let Some(limit) = oplog_limit {
        // mongorestore oplogLimit 형식: <seconds>[:<ordinal>], 미만(<) 의미(oplog_limit_arg가 +1 보정).
        cmd.arg("--oplogLimit").arg(limit);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        XBackupError::Failure(format!("'{program}' 실행 실패(설치/PATH 확인): {e}"))
    })?;

    // stderr를 독립 task로 끝까지 drain(데드락 방지·진단 tail 회수).
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| XBackupError::Failure("mongorestore stderr 파이프 획득 실패".into()))?;
    let drain = tokio::spawn(drain_stderr(stderr));

    let status = child
        .wait()
        .await
        .map_err(|e| XBackupError::Failure(format!("mongorestore wait 실패: {e}")))?;
    let tail = drain.await.unwrap_or_default();

    if status.success() {
        Ok(())
    } else {
        let code = status
            .code()
            .map(|c| c.to_string())
            .unwrap_or_else(|| "signal".into());
        let detail = if tail.is_empty() {
            String::new()
        } else {
            format!(" — 마지막 stderr: {}", tail.join(" | "))
        };
        Err(XBackupError::Failure(format!(
            "PITR oplog 재생 실패(mongorestore exit {code}){detail}"
        )))
    }
}

/// stderr를 끝까지 읽어 tracing으로 흘리고 마지막 5줄을 반환한다(진단용).
async fn drain_stderr(stderr: tokio::process::ChildStderr) -> Vec<String> {
    use tokio::io::{AsyncBufReadExt, BufReader};
    const TAIL: usize = 5;
    let mut reader = BufReader::new(stderr).lines();
    let mut tail: Vec<String> = Vec::new();
    while let Ok(Some(line)) = reader.next_line().await {
        tracing::debug!(target: "mongorestore", "{line}");
        tail.push(line);
        if tail.len() > TAIL {
            tail.remove(0);
        }
    }
    tail
}

/// base + 재생 슬라이스들의 저장 바이트 합을 근사한다(읽기 실패는 0으로 무시).
async fn estimate_bytes(
    store: &ManifestStore<'_>,
    base_id: &str,
    replay_ids: &[String],
) -> u64 {
    let mut total = 0u64;
    if let Ok(m) = store.read(base_id).await {
        total += m.stored_size_bytes;
    }
    for id in replay_ids {
        if let Ok(m) = store.read(id).await {
            total += m.stored_size_bytes;
        }
    }
    total
}

/// id로 노드를 찾는다.
fn find_node<'a>(nodes: &'a [ChainNode], id: &str) -> Option<&'a ChainNode> {
    nodes.iter().find(|n| n.id == id)
}

/// oplog ts(초)를 사람이 읽는 UTC RFC3339(초 단위)로 변환한다(보고용).
fn wall_clock_of(ts: OplogTimestamp) -> String {
    wall_clock_of_unix(ts.t as i64)
}

/// unix 초를 UTC RFC3339(초 단위)로 변환한다.
fn wall_clock_of_unix(unix: i64) -> String {
    DateTime::<Utc>::from_timestamp(unix, 0)
        .map(|dt| dt.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| format!("(unix {unix})"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::BackupStatus;

    fn ts(t: u32, i: u32) -> OplogTimestamp {
        OplogTimestamp::new(t, i)
    }

    fn full(id: &str, end: OplogTimestamp) -> ChainNode {
        ChainNode {
            id: id.to_string(),
            backup_type: BackupType::Full,
            base_id: None,
            oplog_range: Some((end, end)),
            is_empty_slice: false,
            selective: false,
            status: BackupStatus::Complete,
        }
    }

    fn incr(id: &str, base: &str, start: OplogTimestamp, end: OplogTimestamp) -> ChainNode {
        ChainNode {
            id: id.to_string(),
            backup_type: BackupType::Incremental,
            base_id: Some(base.to_string()),
            oplog_range: Some((start, end)),
            is_empty_slice: start == end,
            selective: false,
            status: BackupStatus::Complete,
        }
    }

    // ── PITR + --only 거부(pitfall 1-4, exit 2) ──

    #[test]
    fn pitr_rejects_only_with_usage_exit() {
        let err = reject_pitr_with_only(Some("testdb.items")).unwrap_err();
        assert_eq!(err.exit_code(), 2, "PITR+--only는 exit 2여야 함");
        assert!(err.to_string().contains("--only"), "사유 누락: {err}");
    }

    #[test]
    fn pitr_allows_absent_only() {
        assert!(reject_pitr_with_only(None).is_ok());
    }

    // ── --at 파싱 ──

    #[test]
    fn parse_at_accepts_rfc3339_utc() {
        let dt = parse_at("2026-06-12T13:00:00Z").unwrap();
        assert_eq!(dt.timestamp(), 1_781_269_200);
    }

    #[test]
    fn parse_at_accepts_offset_and_normalizes_to_utc() {
        // +09:00 = 04:00:00Z 동일 순간.
        let z = parse_at("2026-06-12T13:00:00+09:00").unwrap();
        let u = parse_at("2026-06-12T04:00:00Z").unwrap();
        assert_eq!(z.timestamp(), u.timestamp());
    }

    #[test]
    fn parse_at_rejects_garbage_with_usage_exit() {
        let err = parse_at("not-a-time").unwrap_err();
        assert_eq!(err.exit_code(), 2); // Usage.
    }

    // ── oplogLimit 보정(미만 의미 → 목표 초 전체 포함, FR-3 이하 매핑) ──

    #[test]
    fn oplog_limit_uses_next_second_for_inclusive_lower_mapping() {
        // 목표 초 1000의 모든 변경 포함 → 한계 {1001,0}(미만 → 1000:* 전부 포함).
        assert_eq!(oplog_limit_for_second(1000), "1001:0");
        assert_eq!(oplog_limit_for_second(42), "43:0");
    }

    #[test]
    fn oplog_limit_handles_second_overflow() {
        // target_secs == u32::MAX면 saturating(한계를 그대로 둔다).
        assert_eq!(oplog_limit_for_second(u32::MAX), format!("{}:0", u32::MAX));
    }

    // ── --at 내림 매핑 ──

    #[test]
    fn map_target_replays_whole_slice_when_end_below_target() {
        // base(end=100) → i1(100→150) → i2(150→200). target=250 → 전부 재생, 한계 없음.
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let i2 = incr("i2", "base", ts(150, 2), ts(200, 3));
        let nodes = [base.clone(), i1.clone(), i2.clone()];
        let slices = ordered_slices(&nodes, &["i1".into(), "i2".into()]);
        let m = map_target_to_end(&base, &slices, 250);
        assert_eq!(m.replay_ids, vec!["i1", "i2"]);
        assert!(m.limit_slice_id.is_none(), "목표가 마지막 end 이상이면 한계 없음");
        assert_eq!(m.decided_ts, ts(200, 3));
    }

    #[test]
    fn map_target_limits_at_straddling_slice() {
        // target=160 → i1(100→150) 전부, i2(150→200)가 목표를 걸침 → i2에 한계.
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let i2 = incr("i2", "base", ts(150, 2), ts(200, 3));
        let nodes = [base.clone(), i1.clone(), i2.clone()];
        let slices = ordered_slices(&nodes, &["i1".into(), "i2".into()]);
        let m = map_target_to_end(&base, &slices, 160);
        assert_eq!(m.replay_ids, vec!["i1", "i2"]);
        assert_eq!(m.limit_slice_id.as_deref(), Some("i2"));
        assert_eq!(m.decided_ts, ts(160, 0)); // 보고용: 목표 초.
        // 한계 인자는 다음 초(미만) → 목표 초 160의 모든 i 포함, 161+ 제외.
        assert_eq!(m.oplog_limit.as_deref(), Some("161:0"));
    }

    #[test]
    fn map_target_excludes_future_slices() {
        // target=120 → i1(100→150)이 목표를 걸침(한계), i2는 미래라 재생 안 함.
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let i2 = incr("i2", "base", ts(150, 2), ts(200, 3));
        let nodes = [base.clone(), i1.clone(), i2.clone()];
        let slices = ordered_slices(&nodes, &["i1".into(), "i2".into()]);
        let m = map_target_to_end(&base, &slices, 120);
        assert_eq!(m.replay_ids, vec!["i1"], "미래 슬라이스 i2는 제외");
        assert_eq!(m.limit_slice_id.as_deref(), Some("i1"));
    }

    #[test]
    fn map_target_skips_empty_slices() {
        // 빈 슬라이스(변경 없음)는 재생 대상 아님 — 체인 연속성 마커일 뿐(지침 5).
        let base = full("base", ts(100, 1));
        let empty = incr("e", "base", ts(100, 1), ts(100, 1)); // 빈 슬라이스.
        let i2 = incr("i2", "base", ts(100, 1), ts(200, 3));
        let nodes = [base.clone(), empty.clone(), i2.clone()];
        let slices = ordered_slices(&nodes, &["e".into(), "i2".into()]);
        let m = map_target_to_end(&base, &slices, 250);
        assert_eq!(m.replay_ids, vec!["i2"], "빈 슬라이스는 재생 목록에서 제외");
    }

    #[test]
    fn map_target_base_only_when_no_slices_below() {
        // target=90 → base end(100)보다도 이전. 슬라이스 없음, 종료 ts = base end.
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let nodes = [base.clone(), i1.clone()];
        let slices = ordered_slices(&nodes, &["i1".into()]);
        let m = map_target_to_end(&base, &slices, 90);
        // i1.start.t(100) > target(90) → i1 미재생.
        assert!(m.replay_ids.is_empty());
        assert_eq!(m.decided_ts, ts(100, 1)); // base end.
    }

    // ── base 선택(--at 직전 풀백업) ──

    #[test]
    fn select_base_picks_latest_full_before_target() {
        let old = full("00-old", ts(100, 1));
        let new = full("ff-new", ts(200, 1));
        let nodes = [old, new];
        // target=250 → 둘 다 이전. 더 최신(end 큰) ff-new.
        let id = select_base_before(&nodes, 250).unwrap();
        assert_eq!(id, "ff-new");
    }

    #[test]
    fn select_base_excludes_full_after_target() {
        let old = full("00-old", ts(100, 1));
        let future = full("ff-future", ts(500, 1));
        let nodes = [old, future];
        // target=300 → future(end.t=500)는 목표 이후라 제외 → 00-old.
        let id = select_base_before(&nodes, 300).unwrap();
        assert_eq!(id, "00-old");
    }

    #[test]
    fn select_base_errors_when_none_before_target() {
        let future = full("ff-future", ts(500, 1));
        let nodes = [future];
        let err = select_base_before(&nodes, 100).unwrap_err();
        assert_eq!(err.exit_code(), 1); // Failure — base 없음.
    }

    #[test]
    fn select_base_skips_selective_full() {
        let mut sel = full("sel", ts(100, 1));
        sel.selective = true;
        let ok = full("ok", ts(90, 1));
        let nodes = [sel, ok];
        let id = select_base_before(&nodes, 250).unwrap();
        assert_eq!(id, "ok", "selective 풀백업은 PITR base 부적격");
    }

    // ── 체인 거부 메시지 ──

    #[test]
    fn chain_rejection_mentions_verify_chain() {
        // gap이 있는 체인: i2가 i1.end와 불연속.
        let base = full("base", ts(100, 1));
        let i1 = incr("i1", "base", ts(100, 1), ts(150, 2));
        let i2 = incr("i2", "base", ts(160, 0), ts(200, 3)); // gap.
        let nodes = [base, i1, i2];
        let report = verify_chain(&nodes, "base");
        assert!(!report.is_continuous());
        let err = chain_rejection_error(&report);
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("verify"), "안내 누락: {err}");
        assert!(err.to_string().contains("PITR"), "사유 누락: {err}");
    }

    // ── wall-clock 보고 ──

    #[test]
    fn wall_clock_formats_utc_seconds() {
        // 1781269200 = 2026-06-12T13:00:00Z.
        assert_eq!(wall_clock_of(ts(1_781_269_200, 7)), "2026-06-12T13:00:00Z");
    }
}
