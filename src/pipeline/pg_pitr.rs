//! PostgreSQL 시점 복구(PITR) — base 풀백업 복원 후 증분(`xb-pg-incr-v1`) 슬라이스를 목표
//! 시점까지 DML로 재생한다(Mongo PITR의 PG 대응).
//!
//! ```text
//! --at(RFC3339 UTC | "latest") → max_commit_micros
//!   → base 풀백업 선택(최신 Complete PG 풀백업; --id로 고정 가능)
//!   → base 풀 복원(reverse stack → pg_restore)
//!   → base에 체인된 증분들을 id 순으로 수집(archive_format=xb-pg-incr-v1)
//!   → 각 증분: reverse stack → apply(commit ts <= 목표만; "latest"면 전부)
//! ```
//!
//! 증분 적용은 idempotent(I=upsert, U·D=키 기반)라 base와의 겹침·재실행이 안전하다.
//! 시점 경계는 각 변경의 commit 타임스탬프(아카이브에 내장)로 거른다 — manifest의 LSN이
//! 아니라 변경 단위라 PITR 정밀도가 마이크로초까지 간다.

use chrono::{DateTime, Utc};

use crate::config::secret::Secret;
use crate::engine::postgres::{conn::PgClient, incremental, restore::restore_into};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{BackupStatus, BackupType};
use crate::manifest::store::{data_path, ManifestStore};
use crate::pipeline::stage::reverse_stack_for;
use crate::storage::{BoxAsyncRead, Storage};

/// PG PITR 요청.
pub struct PgPitrRequest {
    /// 복구 대상 PostgreSQL URI.
    pub target_uri: Secret,
    /// 목표 시점 — RFC3339 UTC(예: `2026-06-14T09:00:00Z`) 또는 `latest`(전체 재생).
    pub at: String,
    /// base 풀백업 고정(`--id`). 없으면 최신 Complete PG 풀백업.
    pub base_id: Option<String>,
    /// 기존 데이터 덮어쓰기 허용(`--force`).
    pub force: bool,
    /// 계획만 출력(무변경).
    pub dry_run: bool,
    /// 접속 타임아웃(초).
    pub timeout_secs: Option<u64>,
}

/// PG PITR 계획(dry-run·보고용).
#[derive(Debug, Clone)]
pub struct PgPitrPlan {
    /// base 풀백업 ID.
    pub base_id: String,
    /// 재생 대상 증분 ID들(id 순).
    pub incremental_ids: Vec<String>,
    /// 목표 commit 마이크로초(None=latest, 전체 재생).
    pub max_commit_micros: Option<i64>,
    /// 목표 wall-clock 표시(latest면 "latest").
    pub target_label: String,
    /// 대상에 이미 존재하는 사용자 테이블(덮어쓰기 가드용).
    pub conflicting_tables: Vec<String>,
}

/// PG PITR 결과.
#[derive(Debug, Clone)]
pub struct PgPitrOutcome {
    /// 계획.
    pub plan: PgPitrPlan,
    /// 실제 적용한 증분 변경 레코드 총수.
    pub applied_changes: u64,
    /// 재생한 증분 슬라이스 수.
    pub replayed_slices: usize,
}

/// `--at`을 목표 commit 마이크로초로 파싱한다(`latest`면 None=전체 재생).
fn parse_at(at: &str) -> Result<Option<i64>> {
    if at.eq_ignore_ascii_case("latest") {
        return Ok(None);
    }
    let dt = DateTime::parse_from_rfc3339(at).map_err(|e| {
        XBackupError::Usage(crate::tr!("failed to parse --at ('{at}'): {e} — RFC3339 (e.g. 2026-06-14T09:00:00Z) or 'latest' is required", "--at 시각 파싱 실패('{at}'): {e} — RFC3339(예: 2026-06-14T09:00:00Z) 또는 'latest'가 \
             필요합니다"))
    })?;
    Ok(Some(dt.with_timezone(&Utc).timestamp_micros()))
}

/// PG PITR을 실행한다(계획 수립 → 가드 → base 복원 → 증분 재생).
///
/// `confirm`은 비-dry-run에서 충돌(기존 데이터)이 있고 `--force`가 없을 때 TTY 확인에 쓴다.
pub async fn run_pg_pitr<C>(
    request: &PgPitrRequest,
    storage: &dyn Storage,
    is_tty: bool,
    confirm: C,
) -> Result<PgPitrOutcome>
where
    C: FnOnce(&[String]) -> bool,
{
    let max_commit_micros = parse_at(&request.at)?;
    let target_label = match max_commit_micros {
        None => "latest".to_string(),
        Some(_) => request.at.clone(),
    };

    // 1) base 풀백업 + 체인된 증분 수집.
    let store = ManifestStore::new(storage);
    let base_id = match &request.base_id {
        Some(id) => {
            // 지정 id가 적격 PG 풀백업인지 확인.
            let m = store.read(id).await?;
            if !is_pg_full(&m) {
                return Err(XBackupError::Usage(crate::tr!(
                    "--id '{id}' is not a PostgreSQL full backup (not eligible as a PITR base)",
                    "--id '{id}'는 PG 풀백업이 아닙니다(PITR base 부적격)"
                )));
            }
            id.clone()
        }
        None => latest_pg_full(storage).await?,
    };
    let incremental_ids = collect_pg_increments(storage, &base_id).await?;

    // 2) 대상 연결 + 충돌(기존 사용자 테이블) 조회.
    let target = PgClient::connect(&request.target_uri, request.timeout_secs).await?;
    let conflicting_tables = crate::engine::postgres::meta::list_qualified(target.client())
        .await
        .unwrap_or_default();

    let plan = PgPitrPlan {
        base_id: base_id.clone(),
        incremental_ids: incremental_ids.clone(),
        max_commit_micros,
        target_label,
        conflicting_tables: conflicting_tables.clone(),
    };

    // 3) dry-run — 계획만 반환(무변경).
    if request.dry_run {
        tracing::info!(base_id = %base_id, slices = incremental_ids.len(), "PG PITR dry-run — 계획만");
        return Ok(PgPitrOutcome {
            plan,
            applied_changes: 0,
            replayed_slices: 0,
        });
    }

    // 4) 덮어쓰기 가드 — 충돌이 있고 --force 없으면 TTY 확인, 비-TTY면 거부.
    let drop_existing = decide_guard(&conflicting_tables, request.force, is_tty, confirm)?;

    // 5) base 풀 복원(reverse stack → pg_restore). drop은 가드 결과.
    let base_manifest = store.read(&base_id).await?;
    {
        let stages = reverse_stack_for(&base_manifest)?;
        let raw = storage.get_stream(&data_path(&base_id)).await?;
        let mut restored: BoxAsyncRead = stages.apply(raw);
        let inserted = restore_into(&mut restored, target.client(), drop_existing).await?;
        tracing::info!(base_id = %base_id, inserted, "PG PITR — base 풀 복원 완료");
    }

    // 6) 증분 재생(id 순). 한 클라이언트로 이어 적용(session_replication_role=replica는 apply가 설정).
    let mut applied_changes = 0u64;
    let mut replayed = 0usize;
    for id in &incremental_ids {
        let m = store.read(id).await?;
        // 빈 슬라이스(oplog_count=Some(0))는 data.bin이 없다 — 건너뛴다.
        if matches!(m.oplog_count, Some(0)) {
            tracing::debug!(id = %id, "빈 증분 슬라이스 — 건너뜀");
            continue;
        }
        let stages = reverse_stack_for(&m)?;
        let raw = storage.get_stream(&data_path(id)).await?;
        let mut reader: BoxAsyncRead = stages.apply(raw);
        let n = incremental::apply(&mut reader, target.client(), max_commit_micros).await?;
        applied_changes += n;
        replayed += 1;
        tracing::info!(id = %id, changes = n, "PG PITR — 증분 슬라이스 재생");
    }

    tracing::info!(
        base_id = %base_id,
        slices = replayed,
        changes = applied_changes,
        "PG PITR 복구 완료"
    );
    Ok(PgPitrOutcome {
        plan,
        applied_changes,
        replayed_slices: replayed,
    })
}

/// 덮어쓰기 가드 — drop을 켤지 결정하거나 거부한다(Mongo restore::decide_guard와 동형).
fn decide_guard<C>(conflicts: &[String], force: bool, is_tty: bool, confirm: C) -> Result<bool>
where
    C: FnOnce(&[String]) -> bool,
{
    if force {
        return Ok(true);
    }
    if conflicts.is_empty() {
        return Ok(false);
    }
    if is_tty {
        if confirm(conflicts) {
            Ok(true)
        } else {
            Err(XBackupError::Failure(crate::tr!(
                "the user cancelled the restore (existing data preserved)",
                "사용자가 복구를 취소했습니다(기존 데이터 보존)"
            )))
        }
    } else {
        Err(XBackupError::Failure(crate::tr!("the restore target already has data ({} table(s)). Non-TTY refuses to overwrite without --force", "복원 대상에 기존 데이터가 있습니다({}개 테이블). 비-TTY에서는 --force 없이 \
             덮어쓰기를 거부합니다", conflicts.len())))
    }
}

/// manifest가 적격 PG 풀백업인가(Complete·비-selective·archive_format=xb-pg-v1).
fn is_pg_full(m: &crate::manifest::schema::BackupManifest) -> bool {
    matches!(m.backup_type, BackupType::Full)
        && matches!(m.status, BackupStatus::Complete)
        && !m.selective
        && m.tool_versions.archive_format.as_deref()
            == Some(crate::engine::postgres::archive::FORMAT_ID)
}

/// 가장 최신 Complete PG 풀백업 ID.
async fn latest_pg_full(storage: &dyn Storage) -> Result<String> {
    let store = ManifestStore::new(storage);
    let mut ids = crate::pipeline::verify::collect_manifest_ids(storage).await?;
    ids.sort();
    ids.reverse();
    for id in &ids {
        if let Ok(m) = store.read(id).await {
            if is_pg_full(&m) {
                return Ok(m.id);
            }
        }
    }
    Err(XBackupError::Usage(crate::tr!(
        "no PostgreSQL full backup to restore — run a full backup first",
        "복구할 PG 풀백업이 없습니다 — 먼저 풀 백업을 수행하세요"
    )))
}

/// base에 체인된 PG 증분(`xb-pg-incr-v1`) ID를 id 순(=시간 순)으로 모은다.
async fn collect_pg_increments(storage: &dyn Storage, base_id: &str) -> Result<Vec<String>> {
    let store = ManifestStore::new(storage);
    let mut ids = crate::pipeline::verify::collect_manifest_ids(storage).await?;
    ids.sort(); // UUID v7 사전순=생성순(증분 적용 순서).
    let mut out = Vec::new();
    for id in &ids {
        let m = match store.read(id).await {
            Ok(m) => m,
            Err(_) => continue,
        };
        let is_incr = matches!(m.backup_type, BackupType::Incremental)
            && matches!(m.status, BackupStatus::Complete)
            && m.base_id.as_deref() == Some(base_id)
            && m.tool_versions.archive_format.as_deref() == Some(incremental::INCR_FORMAT_ID);
        if is_incr {
            out.push(m.id);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_at_latest_is_none() {
        assert_eq!(parse_at("latest").unwrap(), None);
        assert_eq!(parse_at("LATEST").unwrap(), None);
    }

    #[test]
    fn parse_at_rfc3339_to_micros() {
        let micros = parse_at("2026-06-14T00:00:00Z").unwrap().unwrap();
        // 2026-06-14T00:00:00Z의 epoch 초 * 1_000_000.
        let secs = DateTime::parse_from_rfc3339("2026-06-14T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(micros, secs * 1_000_000);
    }

    #[test]
    fn parse_at_invalid_is_usage_error() {
        let err = parse_at("not-a-time").unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn guard_force_allows_drop() {
        assert!(decide_guard(&["a.b".into()], true, false, |_| false).unwrap());
    }

    #[test]
    fn guard_empty_target_no_drop() {
        assert!(!decide_guard(&[], false, false, |_| true).unwrap());
    }

    #[test]
    fn guard_non_tty_conflict_rejects() {
        let err = decide_guard(&["a.b".into()], false, false, |_| true).unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }

    #[test]
    fn guard_tty_confirm_allows() {
        assert!(decide_guard(&["a.b".into()], false, true, |_| true).unwrap());
    }

    #[test]
    fn guard_tty_decline_rejects() {
        let err = decide_guard(&["a.b".into()], false, true, |_| false).unwrap_err();
        assert_eq!(err.exit_code(), 1);
    }
}
