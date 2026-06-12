//! prune 보존 관리 — 체인 안전 삭제(PRD §FR-11, R19).
//!
//! 무한 증가하는 백업 스토리지를 1차에서도 운영 가능하게 하는 최소 삭제 기능이다.
//! 정책 기반 자동 rotation(GFS)은 2차(§12).
//!
//! ## 핵심 불변 — 체인 단위 삭제(FR-11 필수)
//! base 풀백업 + 그 위에 이어지는 증분들은 **하나의 체인**이다. PITR은 체인을
//! 끊김 없이 재생해야 가능하므로, prune은 **체인 단위로만** 삭제·보존한다.
//! - 살아있는(보존되는) 증분이 참조하는 base는 **절대 삭제하지 않는다**.
//! - 체인이 보존 대상이면 base+증분 전부 보존, 삭제 대상이면 base+증분 전부 삭제.
//!
//! ## 보존 규칙
//! - `--keep-full N`: 최신 풀백업 체인 N개를 보존(체인을 base 생성 시각 내림차순으로
//!   정렬해 앞 N개).
//! - `--keep-days D`: 기준 시각(now - D일) 이내에 **생성된 체인**을 보존(체인의
//!   *최신 구성원* 생성 시각이 기준 이내면 보존 — 최근 증분이 붙은 체인은 살린다).
//! - 둘 다 주면 **합집합 보존**(어느 한 규칙이라도 보존하면 보존). 둘 다 미지정이면
//!   안전을 위해 **아무것도 삭제하지 않는다**(명시적 기준 없는 삭제 금지).
//!
//! ## orphan / incomplete
//! - manifest 없는 data(orphan)·incomplete 백업은 체인에 쓸 수 없는 잔재다. 삭제
//!   후보로 표시하되 **`--force`에서만** 삭제한다(보수적 — 운영자 확인 우선).
//!
//! 순수 함수([`plan_prune`])는 I/O 없이 보존/삭제를 판정한다(단위 테스트 용이).
//! 스토리지 연동(수집·실삭제)은 [`load_backups`]·[`execute_prune`]가 담당한다.

use crate::error::{Result, XBackupError};
use crate::manifest::chain::{verify_chain, ChainNode};
use crate::manifest::schema::{BackupManifest, BackupStatus, BackupType};
use crate::manifest::store::{
    data_path, manifest_path, manifest_sha_path, ManifestStore, DATA_FILE, MANIFEST_FILE,
};
use crate::pipeline::verify::collect_manifest_ids;
use crate::storage::Storage;

/// 보존 규칙(CLI `--keep-full`/`--keep-days`에서 매핑).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RetentionPolicy {
    /// 최신 풀백업 체인 N개 보존(None이면 이 규칙 미적용).
    pub keep_full: Option<u32>,
    /// 최근 D일 이내 생성 체인 보존(None이면 이 규칙 미적용).
    pub keep_days: Option<u32>,
}

impl RetentionPolicy {
    /// 보존 기준이 하나도 없으면 true(이 경우 아무것도 삭제하지 않는다).
    pub fn is_unspecified(&self) -> bool {
        self.keep_full.is_none() && self.keep_days.is_none()
    }
}

/// 삭제 후보의 분류(사유 표시·삭제 게이팅에 사용).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PruneKind {
    /// 보존 규칙에 의해 만료된 정상 체인.
    Chain,
    /// manifest 없이 data만 있는 유령 산출물(--force 필요).
    Orphan,
    /// incomplete 백업(체인 부적격, --force 필요).
    Incomplete,
}

impl PruneKind {
    /// 이 분류가 `--force` 없이도 삭제 가능한지(정상 체인만 대화형/force로 삭제).
    fn is_normal_chain(&self) -> bool {
        matches!(self, PruneKind::Chain)
    }
}

/// 삭제 대상 한 단위(체인 또는 단일 잔재).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PruneTarget {
    /// 분류(chain/orphan/incomplete).
    pub kind: PruneKind,
    /// base 풀백업 ID(체인일 때). orphan/incomplete는 자기 자신.
    pub base_id: String,
    /// 이 단위에 포함된 모든 백업 ID(삭제 단위). 체인은 base+증분 전체.
    pub member_ids: Vec<String>,
    /// 사람이 읽는 삭제 사유.
    pub reason: String,
}

/// prune 계획 — 보존/삭제 판정 결과(순수).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PrunePlan {
    /// 삭제 대상 단위들.
    pub targets: Vec<PruneTarget>,
    /// 보존된 체인의 base ID(진단·dry-run 출력용).
    pub kept_base_ids: Vec<String>,
}

impl PrunePlan {
    /// 삭제할 백업이 하나도 없으면 true.
    pub fn is_empty(&self) -> bool {
        self.targets.is_empty()
    }

    /// 정상 체인(force 불요) 삭제 대상이 하나라도 있는지.
    pub fn has_normal_chain_targets(&self) -> bool {
        self.targets.iter().any(|t| t.kind.is_normal_chain())
    }

    /// orphan/incomplete(force 필요) 대상이 하나라도 있는지.
    pub fn has_force_only_targets(&self) -> bool {
        self.targets.iter().any(|t| !t.kind.is_normal_chain())
    }

    /// 실제로 삭제할 백업 ID를 모두 평탄화한다(--force 게이팅 후).
    ///
    /// `include_force_only=false`면 정상 체인만, true면 orphan/incomplete까지 포함한다.
    pub fn deletable_member_ids(&self, include_force_only: bool) -> Vec<String> {
        self.targets
            .iter()
            .filter(|t| include_force_only || t.kind.is_normal_chain())
            .flat_map(|t| t.member_ids.iter().cloned())
            .collect()
    }
}

/// 보존/삭제를 판정하는 **순수 함수**(I/O 없음).
///
/// - `manifests`: destination의 모든 백업 manifest.
/// - `orphan_ids`: manifest 없이 data만 있는 디렉터리 ID(orphan).
/// - `policy`: 보존 규칙.
/// - `now_secs`: 기준 "지금"(Unix epoch 초). `--keep-days` 판정에 쓴다(테스트 주입).
///
/// 체인 구성은 [`verify_chain`](crate::manifest::chain::verify_chain)이 식별하는
/// base+증분 그룹을 그대로 따른다(같은 base를 가리키는 증분 = 같은 체인). 체인 단위로
/// 보존/삭제를 결정하므로, 살아있는 증분이 참조하는 base는 절대 단독 삭제되지 않는다.
pub fn plan_prune(
    manifests: &[BackupManifest],
    orphan_ids: &[String],
    policy: RetentionPolicy,
    now_secs: i64,
) -> PrunePlan {
    let nodes: Vec<ChainNode> = manifests.iter().map(ChainNode::from_manifest).collect();

    // 1) 체인 그룹핑 — base별로 (base + 그 base를 가리키는 증분) 묶음을 만든다.
    //    incomplete가 아닌 풀백업을 base 후보로 삼는다.
    let mut chains: Vec<ChainGroup> = Vec::new();
    for m in manifests {
        if m.backup_type != BackupType::Full {
            continue; // 증분은 자기 base 그룹에 흡수된다(아래).
        }
        if m.status == BackupStatus::Incomplete {
            continue; // incomplete 풀은 체인 base 부적격 → 잔재로 따로 처리.
        }
        // verify_chain으로 이 base에 연결된 증분(complete만 의미 있음)을 모은다.
        let report = verify_chain(&nodes, &m.id);
        let mut members = vec![m.id.clone()];
        // report.incremental_ids는 같은 base를 가리키는 증분(정렬됨). incomplete 증분도
        // 체인 멤버로 포함해야 한다 — 그래야 base 삭제 시 함께 정리되어 잔재가 안 남는다.
        members.extend(report.incremental_ids.iter().cloned());
        // 체인의 최신 구성원 생성 시각(=keep-days 기준)·base 생성 시각(=keep-full 정렬 기준).
        let base_created = created_secs(m).unwrap_or(i64::MIN);
        let newest_created = members
            .iter()
            .filter_map(|id| manifests.iter().find(|x| &x.id == id))
            .filter_map(created_secs)
            .max()
            .unwrap_or(base_created);
        chains.push(ChainGroup {
            base_id: m.id.clone(),
            member_ids: members,
            base_created,
            newest_created,
        });
    }

    // 2) keep-full 정렬 — base 생성 시각 내림차순(최신 우선). 동률은 base_id 사전순.
    chains.sort_by(|a, b| {
        b.base_created
            .cmp(&a.base_created)
            .then(a.base_id.cmp(&b.base_id))
    });

    // 3) 각 체인의 보존 여부를 합집합 규칙으로 판정한다.
    let mut targets = Vec::new();
    let mut kept_base_ids = Vec::new();
    let keep_days_cutoff = policy.keep_days.map(|d| now_secs - (d as i64) * 86_400);

    for (idx, chain) in chains.iter().enumerate() {
        // 규칙 미지정이면 전부 보존(삭제 금지).
        if policy.is_unspecified() {
            kept_base_ids.push(chain.base_id.clone());
            continue;
        }
        // keep-full: 정렬 앞에서 N개 보존.
        let kept_by_full = policy.keep_full.is_some_and(|n| idx < n as usize);
        // keep-days: 체인 최신 구성원이 기준 이내면 보존.
        let kept_by_days = keep_days_cutoff.is_some_and(|cut| chain.newest_created >= cut);

        if kept_by_full || kept_by_days {
            kept_base_ids.push(chain.base_id.clone());
        } else {
            let reason = prune_reason(policy, idx);
            targets.push(PruneTarget {
                kind: PruneKind::Chain,
                base_id: chain.base_id.clone(),
                member_ids: chain.member_ids.clone(),
                reason,
            });
        }
    }

    // 4) incomplete 백업(체인 base가 아닌 잔재) — orphan/incomplete로 따로 표시.
    //    체인에 멤버로 흡수된 incomplete 증분은 제외한다(이미 위에서 다뤘다).
    let chain_members: std::collections::HashSet<&String> =
        chains.iter().flat_map(|c| c.member_ids.iter()).collect();
    for m in manifests {
        if m.status != BackupStatus::Incomplete {
            continue;
        }
        if chain_members.contains(&m.id) {
            continue; // 이미 어떤 체인의 멤버.
        }
        targets.push(PruneTarget {
            kind: PruneKind::Incomplete,
            base_id: m.id.clone(),
            member_ids: vec![m.id.clone()],
            reason: "incomplete 백업(체인 부적격 잔재) — --force에서만 삭제".to_string(),
        });
    }

    // 5) orphan(manifest 없는 data) — 잔재로 표시(--force 필요).
    for id in orphan_ids {
        targets.push(PruneTarget {
            kind: PruneKind::Orphan,
            base_id: id.clone(),
            member_ids: vec![id.clone()],
            reason: "orphan(manifest 없는 data 산출물) — --force에서만 삭제".to_string(),
        });
    }

    PrunePlan {
        targets,
        kept_base_ids,
    }
}

/// 체인 그룹(내부 — base와 그 증분 멤버).
struct ChainGroup {
    base_id: String,
    member_ids: Vec<String>,
    /// base 생성 시각(Unix 초) — keep-full 정렬 기준.
    base_created: i64,
    /// 체인 최신 구성원 생성 시각(Unix 초) — keep-days 판정 기준.
    newest_created: i64,
}

/// 삭제 사유 문자열을 만든다(어떤 규칙이 만료시켰는지).
fn prune_reason(policy: RetentionPolicy, idx: usize) -> String {
    match (policy.keep_full, policy.keep_days) {
        (Some(n), Some(d)) => format!(
            "보존 기준 초과: keep-full {n}(정렬 {}번째)·keep-days {d}일 모두 벗어남",
            idx + 1
        ),
        (Some(n), None) => format!("keep-full {n} 초과(정렬 {}번째 체인)", idx + 1),
        (None, Some(d)) => format!("keep-days {d}일 이전에 생성된 체인"),
        (None, None) => "보존 기준 미지정".to_string(), // 도달하지 않음(is_unspecified 가드).
    }
}

/// manifest의 `created_at`(RFC3339)을 Unix 초로 파싱한다(실패면 None).
fn created_secs(m: &BackupManifest) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(&m.created_at)
        .ok()
        .map(|dt| dt.timestamp())
}

/// destination에서 prune 판정 입력(manifest 목록 + orphan ID)을 수집한다.
pub async fn load_backups(storage: &dyn Storage) -> Result<(Vec<BackupManifest>, Vec<String>)> {
    let store = ManifestStore::new(storage);

    // manifest를 가진 ID 수집·로드.
    let ids = collect_manifest_ids(storage).await?;
    let mut manifests = Vec::with_capacity(ids.len());
    for id in &ids {
        match store.read(id).await {
            Ok(m) => manifests.push(m),
            Err(e) => {
                // 읽기 실패한 manifest는 prune 판정에서 제외한다(섣불리 삭제하지 않음).
                tracing::warn!(id = %id, "manifest 읽기 실패(prune 판정 제외): {e}");
            }
        }
    }

    // orphan 수집 — manifest 없이 data.bin만 있는 디렉터리.
    let orphan_ids = detect_orphan_ids(storage, &ids).await?;

    Ok((manifests, orphan_ids))
}

/// manifest 없는 data.bin 디렉터리 ID를 모은다(list.rs detect_orphans와 동일 기준).
async fn detect_orphan_ids(storage: &dyn Storage, known_ids: &[String]) -> Result<Vec<String>> {
    let entries = storage.list("").await?;
    let data_suffix = format!("/{DATA_FILE}");
    let manifest_suffix = format!("/{MANIFEST_FILE}");

    let mut orphans = Vec::new();
    for e in &entries {
        let Some(id) = e
            .path
            .strip_suffix(&data_suffix)
            .filter(|id| !id.is_empty() && !id.contains('/'))
        else {
            continue;
        };
        if known_ids.iter().any(|k| k == id) {
            continue;
        }
        // manifest 파일이 있으면(읽기만 실패) orphan으로 보지 않는다.
        let has_manifest_file = entries
            .iter()
            .any(|x| x.path == format!("{id}{manifest_suffix}"));
        if has_manifest_file {
            continue;
        }
        orphans.push(id.to_string());
    }
    orphans.sort();
    orphans.dedup();
    Ok(orphans)
}

/// 계획대로 실제 삭제를 수행한다.
///
/// `include_force_only`면 orphan/incomplete까지 삭제한다(--force).
///
/// ## 삭제 순서(부분 실패 시 체인이 깨진 채 남지 않게 — 주석 근거)
/// 체인 단위로 **증분(최신→과거) → base** 순으로 지운다. 그리고 각 백업 안에서는
/// **data.bin → 사이드카 → manifest.json**을 **manifest를 마지막에** 지운다.
/// 근거: manifest가 마지막까지 남아 있으면, 중간에 실패해도 그 백업은 "manifest는
/// 있으나 data가 없는" 상태가 되어 list/verify가 깨짐을 감지할 수 있다. 반대로
/// manifest를 먼저 지우면 data만 남은 orphan이 되어 추적이 어려워진다. 증분을 base보다
/// 먼저 지우는 이유도 같다 — base를 먼저 지우면 남은 증분이 "base 없는 증분"이 되어
/// 더 위험한 끊긴 체인이 된다.
pub async fn execute_prune(
    storage: &dyn Storage,
    plan: &PrunePlan,
    include_force_only: bool,
) -> Result<PruneOutcome> {
    let store_manifests = ManifestStore::new(storage); // (간접 참조 회피용 — 직접 delete 사용)
    let _ = &store_manifests;

    let mut deleted_backups = 0usize;
    let mut deleted_chains = 0usize;

    for target in &plan.targets {
        if !include_force_only && !target.kind.is_normal_chain() {
            continue; // orphan/incomplete는 --force에서만.
        }

        // 체인은 멤버를 증분(최신)→base 순으로 지워야 한다. member_ids[0]이 base이고
        // 그 뒤가 oplog 순서로 정렬된 증분이므로, base를 마지막에 두도록 역순 처리한다.
        for backup_id in delete_order(&target.member_ids) {
            delete_one_backup(storage, &backup_id).await?;
            deleted_backups += 1;
        }
        if target.kind.is_normal_chain() {
            deleted_chains += 1;
        }
    }

    Ok(PruneOutcome {
        deleted_backups,
        deleted_chains,
    })
}

/// 단일 백업의 산출물을 안전 순서로 삭제한다: data.bin → 사이드카 → manifest.json.
///
/// 존재하지 않는 파일(빈 슬라이스의 data 부재 등)은 정상으로 본다(NotFound 무시).
async fn delete_one_backup(storage: &dyn Storage, backup_id: &str) -> Result<()> {
    // 1) data.bin(빈 슬라이스·orphan은 없을 수 있음 — 무해).
    delete_if_exists(storage, &data_path(backup_id)).await?;
    // 2) 사이드카(orphan은 없을 수 있음).
    delete_if_exists(storage, &manifest_sha_path(backup_id)).await?;
    // 3) manifest.json 마지막(중간 실패 시 추적 가능성 유지).
    delete_if_exists(storage, &manifest_path(backup_id)).await?;
    Ok(())
}

/// 경로를 삭제하되, 이미 없으면(NotFound 류) 조용히 성공으로 본다.
///
/// Storage::delete는 백엔드별로 "없음"을 에러로 줄 수 있어, 메시지로 NotFound를
/// 보수적으로 흡수한다(빈 슬라이스 data 부재 등 정상 케이스 처리). 그 외 에러는 전파.
async fn delete_if_exists(storage: &dyn Storage, path: &str) -> Result<()> {
    match storage.delete(path).await {
        Ok(()) => Ok(()),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("not found")
                || msg.contains("No such file")
                || msg.contains("NotFound")
                || msg.contains("찾을 수 없")
            {
                tracing::debug!(path = %path, "삭제 대상이 이미 없음(무시)");
                Ok(())
            } else {
                Err(XBackupError::StorageDownload(format!(
                    "'{path}' 삭제 실패: {e}"
                )))
            }
        }
    }
}

/// 체인 멤버를 삭제 순서(증분 최신→과거, base 마지막)로 정렬해 반환한다.
///
/// `member_ids[0]`은 base, 그 뒤는 oplog 순서(오래된→최신)로 정렬된 증분이다
/// (plan_prune 구성 규칙). 따라서 증분을 역순(최신→과거)으로 먼저, base를 맨 뒤에 둔다.
fn delete_order(member_ids: &[String]) -> Vec<String> {
    if member_ids.len() <= 1 {
        return member_ids.to_vec();
    }
    let base = &member_ids[0];
    let mut order: Vec<String> = member_ids[1..].iter().rev().cloned().collect();
    order.push(base.clone());
    order
}

/// 실삭제 결과 요약.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneOutcome {
    /// 삭제된 백업(개별 manifest) 개수.
    pub deleted_backups: usize,
    /// 삭제된 정상 체인 개수.
    pub deleted_chains: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::schema::{
        BackupManifest, BackupStatus, BackupType, OplogRange, OplogTimestamp, Topology,
        FORMAT_VERSION,
    };

    /// 테스트용 manifest 빌더(시각은 epoch 초 → RFC3339).
    fn mk(id: &str, kind: BackupType, base: Option<&str>, created_secs: i64) -> BackupManifest {
        let created_at = chrono::DateTime::from_timestamp(created_secs, 0)
            .unwrap()
            .to_rfc3339();
        BackupManifest {
            format_version: FORMAT_VERSION,
            id: id.to_string(),
            created_at,
            backup_type: kind,
            base_id: base.map(str::to_string),
            topology: Topology::ReplicaSet,
            server_version: "7.0.35".into(),
            tool_versions: Default::default(),
            selective: false,
            original_size_bytes: 10,
            stored_size_bytes: 10,
            compression: None,
            encryption: None,
            checksum_sha256: "x".into(),
            oplog_range: Some(OplogRange {
                start_ts: OplogTimestamp::new(created_secs as u32, 1),
                end_ts: OplogTimestamp::new(created_secs as u32, 1),
            }),
            oplog_count: None,
            promoted_from_gap: false,
            status: BackupStatus::Complete,
        }
    }

    /// 연속 증분 체인(base end == incr start)을 만든다.
    fn chain(base: &str, base_t: i64, incrs: &[(&str, i64)]) -> Vec<BackupManifest> {
        let mut v = vec![mk(base, BackupType::Full, None, base_t)];
        let mut prev_end = base_t as u32;
        for (id, t) in incrs {
            let mut m = mk(id, BackupType::Incremental, Some(base), *t);
            m.oplog_range = Some(OplogRange {
                start_ts: OplogTimestamp::new(prev_end, 1),
                end_ts: OplogTimestamp::new(*t as u32, 1),
            });
            prev_end = *t as u32;
            v.push(m);
        }
        v
    }

    const DAY: i64 = 86_400;

    /// keep-full 1: 최신 체인 하나만 보존, 나머지는 삭제(체인 단위).
    #[test]
    fn keep_full_retains_newest_chains() {
        let mut all = Vec::new();
        all.extend(chain("base-old", 1_000 * DAY, &[]));
        all.extend(chain("base-new", 2_000 * DAY, &[]));
        let policy = RetentionPolicy {
            keep_full: Some(1),
            keep_days: None,
        };
        let plan = plan_prune(&all, &[], policy, 3_000 * DAY);

        // base-new 보존, base-old 삭제.
        assert_eq!(plan.kept_base_ids, vec!["base-new"]);
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].base_id, "base-old");
        assert_eq!(plan.targets[0].kind, PruneKind::Chain);
    }

    /// 살아있는 증분이 참조하는 base는 체인 보존 시 함께 보존된다(base 단독 삭제 금지).
    #[test]
    fn live_incremental_protects_its_base() {
        // base-old에 최근 증분이 붙어 있다 → keep-days로 체인 전체 보존.
        let mut all = Vec::new();
        all.extend(chain("base-old", 10 * DAY, &[("i-recent", 100 * DAY)]));
        all.extend(chain("base-new", 50 * DAY, &[]));
        // keep-days 30일: now=110일 기준 cutoff=80일. i-recent(100일)가 기준 이내 → base-old 보존.
        let policy = RetentionPolicy {
            keep_full: None,
            keep_days: Some(30),
        };
        let plan = plan_prune(&all, &[], policy, 110 * DAY);

        // base-old는 오래됐지만 최근 증분 때문에 보존, base-new는 50일(>cutoff 80? no 50<80) → 삭제.
        assert!(
            plan.kept_base_ids.contains(&"base-old".to_string()),
            "최근 증분이 base-old를 보호해야 함: {plan:?}"
        );
        // base-new(50일)는 cutoff(80일) 이전 → 삭제 대상.
        let new_pruned = plan
            .targets
            .iter()
            .any(|t| t.base_id == "base-new" && t.kind == PruneKind::Chain);
        assert!(new_pruned, "base-new는 삭제 대상이어야 함: {plan:?}");
    }

    /// 삭제되는 체인은 base+증분 전부를 멤버로 포함한다(체인 단위 삭제).
    #[test]
    fn pruned_chain_includes_base_and_incrementals() {
        let all = chain("base", 10 * DAY, &[("i1", 11 * DAY), ("i2", 12 * DAY)]);
        let policy = RetentionPolicy {
            keep_full: Some(0), // 0개 보존 → 모두 삭제.
            keep_days: None,
        };
        let plan = plan_prune(&all, &[], policy, 100 * DAY);
        assert_eq!(plan.targets.len(), 1);
        let members = &plan.targets[0].member_ids;
        assert!(members.contains(&"base".to_string()));
        assert!(members.contains(&"i1".to_string()));
        assert!(members.contains(&"i2".to_string()));
        assert_eq!(members.len(), 3);
    }

    /// keep-days 합집합: keep-full로 못 살린 체인도 keep-days로 살릴 수 있다.
    #[test]
    fn union_retention_keeps_if_either_rule_keeps() {
        // 체인 3개. keep-full 1이면 newest 1개만 보존하지만, keep-days가 둘째도 살린다.
        let mut all = Vec::new();
        all.extend(chain("c1", DAY, &[])); // 아주 오래됨.
        all.extend(chain("c2", 95 * DAY, &[])); // 최근.
        all.extend(chain("c3", 100 * DAY, &[])); // 가장 최근.
        let policy = RetentionPolicy {
            keep_full: Some(1),  // c3만.
            keep_days: Some(10), // now=105 → cutoff=95 → c2(95)·c3(100) 보존.
        };
        let plan = plan_prune(&all, &[], policy, 105 * DAY);
        // c2, c3 보존(합집합), c1만 삭제.
        assert!(plan.kept_base_ids.contains(&"c2".to_string()));
        assert!(plan.kept_base_ids.contains(&"c3".to_string()));
        assert_eq!(plan.targets.len(), 1);
        assert_eq!(plan.targets[0].base_id, "c1");
    }

    /// 보존 기준 미지정이면 아무것도 삭제하지 않는다(안전).
    #[test]
    fn no_policy_deletes_nothing() {
        let all = chain("base", 10 * DAY, &[("i1", 11 * DAY)]);
        let plan = plan_prune(&all, &[], RetentionPolicy::default(), 100 * DAY);
        assert!(plan.is_empty());
        assert!(plan.kept_base_ids.contains(&"base".to_string()));
    }

    /// orphan은 항상 삭제 후보지만 force-only로 분류된다.
    #[test]
    fn orphan_is_force_only_target() {
        let all = chain("base", 10 * DAY, &[]);
        let policy = RetentionPolicy {
            keep_full: Some(10), // base는 보존.
            keep_days: None,
        };
        let plan = plan_prune(&all, &["ghost".to_string()], policy, 100 * DAY);
        // base는 보존, ghost는 orphan 타깃.
        assert!(plan.kept_base_ids.contains(&"base".to_string()));
        let orphan = plan.targets.iter().find(|t| t.base_id == "ghost").unwrap();
        assert_eq!(orphan.kind, PruneKind::Orphan);
        assert!(plan.has_force_only_targets());
        // force 미포함이면 orphan은 삭제 목록에서 빠진다.
        assert!(plan.deletable_member_ids(false).is_empty());
        assert_eq!(plan.deletable_member_ids(true), vec!["ghost"]);
    }

    /// incomplete 백업(체인 base가 아닌 잔재)은 incomplete force-only 타깃이 된다.
    #[test]
    fn incomplete_standalone_is_force_only() {
        let mut m = mk("inc", BackupType::Full, None, 10 * DAY);
        m.status = BackupStatus::Incomplete;
        let policy = RetentionPolicy {
            keep_full: Some(10),
            keep_days: None,
        };
        let plan = plan_prune(&[m], &[], policy, 100 * DAY);
        let t = plan.targets.iter().find(|t| t.base_id == "inc").unwrap();
        assert_eq!(t.kind, PruneKind::Incomplete);
        assert!(plan.has_force_only_targets());
    }

    /// delete_order: 증분(최신→과거) 후 base 마지막.
    #[test]
    fn delete_order_incrementals_first_base_last() {
        let members = vec![
            "base".to_string(),
            "i1".to_string(),
            "i2".to_string(),
            "i3".to_string(),
        ];
        let order = delete_order(&members);
        // i3, i2, i1, base 순.
        assert_eq!(order, vec!["i3", "i2", "i1", "base"]);
    }

    /// delete_order: 단일 멤버(base만)는 그대로.
    #[test]
    fn delete_order_single_member() {
        assert_eq!(delete_order(&["base".to_string()]), vec!["base"]);
    }
}
