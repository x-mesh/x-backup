//! MySQL 시점 복구(PITR) — base 풀백업 복원 후 증분(`xb-mysql-incr-v1`) 슬라이스를 목표 시점까지
//! 멱등 DML로 재생한다(PG PITR의 MySQL 대응).
//!
//! ```text
//! --at(RFC3339 UTC | "latest") → max_commit_micros
//!   → base 풀백업 선택(최신 Complete MySQL 풀백업; --id로 고정 가능)
//!   → base 풀 복원(reverse stack → mysql restore_into)
//!   → base에 체인된 증분들을 id 순으로 수집(archive_format=xb-mysql-incr-v1)
//!   → 각 증분: reverse stack → apply(commit ts <= 목표만; "latest"면 전부)
//! ```
//!
//! 증분 적용은 멱등(I=upsert, U·D=키 기반)이라 base와의 겹침·재실행이 안전하다. 시점 경계는
//! 각 변경의 commit 타임스탬프로 거른다 — binlog 이벤트 헤더가 **초 단위**라 PITR 정밀도는
//! 1초다(공식 권고대로 정확 컷은 위치/GTID 기반이며, 시간 필터는 보조).

use chrono::{DateTime, Utc};

use crate::config::secret::Secret;
use crate::engine::mysql::{conn::MysqlClient, incremental, meta as my_meta, restore as my_restore};
use crate::error::{Result, XBackupError};
use crate::manifest::schema::{BackupManifest, BackupStatus, BackupType, MysqlBinlogCoords};
use crate::manifest::store::{data_path, ManifestStore};
use crate::pipeline::stage::reverse_stack_for;
use crate::storage::{BoxAsyncRead, Storage};

/// MySQL PITR 요청.
pub struct MysqlPitrRequest {
    pub target_uri: Secret,
    /// 목표 시점 — RFC3339 UTC 또는 `latest`(전체 재생).
    pub at: String,
    /// base 풀백업 고정(`--id`). 없으면 최신 Complete MySQL 풀백업.
    pub base_id: Option<String>,
    pub force: bool,
    pub dry_run: bool,
    pub timeout_secs: Option<u64>,
}

/// MySQL PITR 계획(dry-run·보고용).
#[derive(Debug, Clone)]
pub struct MysqlPitrPlan {
    pub base_id: String,
    pub incremental_ids: Vec<String>,
    pub max_commit_micros: Option<i64>,
    pub target_label: String,
    pub conflicting_tables: Vec<String>,
}

/// MySQL PITR 결과.
#[derive(Debug, Clone)]
pub struct MysqlPitrOutcome {
    pub plan: MysqlPitrPlan,
    pub applied_changes: u64,
    pub replayed_slices: usize,
}

/// `--at`을 목표 commit 마이크로초로 파싱한다(`latest`면 None=전체 재생).
fn parse_at(at: &str) -> Result<Option<i64>> {
    if at.eq_ignore_ascii_case("latest") {
        return Ok(None);
    }
    let dt = DateTime::parse_from_rfc3339(at).map_err(|e| {
        XBackupError::Usage(format!(
            "--at 시각 파싱 실패('{at}'): {e} — RFC3339(예: 2026-06-14T09:00:00Z) 또는 'latest'가 \
             필요합니다"
        ))
    })?;
    Ok(Some(dt.with_timezone(&Utc).timestamp_micros()))
}

/// MySQL PITR을 실행한다(계획 → 가드 → base 복원 → 증분 재생).
pub async fn run_mysql_pitr<C>(
    request: &MysqlPitrRequest,
    storage: &dyn Storage,
    is_tty: bool,
    confirm: C,
) -> Result<MysqlPitrOutcome>
where
    C: FnOnce(&[String]) -> bool,
{
    let max_commit_micros = parse_at(&request.at)?;
    let target_label = match max_commit_micros {
        None => "latest".to_string(),
        Some(_) => request.at.clone(),
    };

    let store = ManifestStore::new(storage);
    let base_id = match &request.base_id {
        Some(id) => {
            let m = store.read(id).await?;
            if !is_mysql_full(&m) {
                return Err(XBackupError::Usage(format!(
                    "--id '{id}'는 MySQL 풀백업이 아닙니다(PITR base 부적격)"
                )));
            }
            id.clone()
        }
        None => latest_mysql_full(storage).await?,
    };
    let incremental_ids = collect_mysql_increments(storage, &base_id).await?;

    let mut target = MysqlClient::connect(&request.target_uri, request.timeout_secs).await?;
    let conflicting_tables = my_meta::list_qualified(target.conn_mut())
        .await
        .unwrap_or_default();

    let plan = MysqlPitrPlan {
        base_id: base_id.clone(),
        incremental_ids: incremental_ids.clone(),
        max_commit_micros,
        target_label,
        conflicting_tables: conflicting_tables.clone(),
    };

    if request.dry_run {
        tracing::info!(base_id = %base_id, slices = incremental_ids.len(), "MySQL PITR dry-run — 계획만");
        return Ok(MysqlPitrOutcome {
            plan,
            applied_changes: 0,
            replayed_slices: 0,
        });
    }

    let drop_existing = decide_guard(&conflicting_tables, request.force, is_tty, confirm)?;

    // base 풀 복원(reverse stack → mysql restore_into).
    let base_manifest = store.read(&base_id).await?;
    {
        let stages = reverse_stack_for(&base_manifest)?;
        let raw = storage.get_stream(&data_path(&base_id)).await?;
        let mut restored: BoxAsyncRead = stages.apply(raw);
        let inserted =
            my_restore::restore_into(&mut restored, target.conn_mut(), drop_existing).await?;
        tracing::info!(base_id = %base_id, inserted, "MySQL PITR — base 풀 복원 완료");
    }

    // 증분 재생(id 순).
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
        let n = incremental::apply(&mut reader, target.conn_mut(), max_commit_micros).await?;
        applied_changes += n;
        replayed += 1;
        tracing::info!(id = %id, changes = n, "MySQL PITR — 증분 슬라이스 재생");
    }

    tracing::info!(base_id = %base_id, slices = replayed, changes = applied_changes, "MySQL PITR 복구 완료");
    Ok(MysqlPitrOutcome {
        plan,
        applied_changes,
        replayed_slices: replayed,
    })
}

/// 덮어쓰기 가드 — drop을 켤지 결정하거나 거부한다(PG PITR과 동형).
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
            Err(XBackupError::Failure(
                "사용자가 복구를 취소했습니다(기존 데이터 보존)".into(),
            ))
        }
    } else {
        Err(XBackupError::Failure(format!(
            "복원 대상에 기존 데이터가 있습니다({}개 테이블). 비-TTY에서는 --force 없이 \
             덮어쓰기를 거부합니다",
            conflicts.len()
        )))
    }
}

/// manifest가 적격 MySQL 풀백업인가(Complete·비-selective·archive_format=xb-mysql-v1).
pub(crate) fn is_mysql_full(m: &BackupManifest) -> bool {
    matches!(m.backup_type, BackupType::Full)
        && matches!(m.status, BackupStatus::Complete)
        && !m.selective
        && m.tool_versions.archive_format.as_deref()
            == Some(crate::engine::mysql::archive::FORMAT_ID)
}

/// 가장 최신 Complete MySQL 풀백업 ID.
pub(crate) async fn latest_mysql_full(storage: &dyn Storage) -> Result<String> {
    let store = ManifestStore::new(storage);
    let mut ids = crate::pipeline::verify::collect_manifest_ids(storage).await?;
    ids.sort();
    ids.reverse();
    for id in &ids {
        if let Ok(m) = store.read(id).await {
            if is_mysql_full(&m) {
                return Ok(m.id);
            }
        }
    }
    Err(XBackupError::Usage(
        "복구할 MySQL 풀백업이 없습니다 — 먼저 풀 백업을 수행하세요".into(),
    ))
}

/// base에 체인된 MySQL 증분(`xb-mysql-incr-v1`) ID를 id 순(=시간 순)으로 모은다.
pub(crate) async fn collect_mysql_increments(
    storage: &dyn Storage,
    base_id: &str,
) -> Result<Vec<String>> {
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

/// 증분 체인의 다음 시작 binlog 좌표 — 마지막 증분의 end 좌표, 없으면 base의 좌표.
pub(crate) async fn chain_start_coords(
    storage: &dyn Storage,
    base_id: &str,
) -> Result<MysqlBinlogCoords> {
    let store = ManifestStore::new(storage);
    let incrs = collect_mysql_increments(storage, base_id).await?;
    // 가장 최근(마지막) 증분의 mysql_binlog가 다음 시작점. id 순 정렬이라 마지막이 최신.
    for id in incrs.iter().rev() {
        if let Ok(m) = store.read(id).await {
            if let Some(coords) = m.mysql_binlog {
                return Ok(coords);
            }
        }
    }
    let base = store.read(base_id).await?;
    base.mysql_binlog.ok_or_else(|| {
        XBackupError::Usage(
            "base 풀백업에 binlog 좌표가 없습니다 — 증분은 log_bin=ON 서버에서 만든 풀백업이 \
             필요합니다(features.incremental.mysql_binlog=true)."
                .into(),
        )
    })
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
        let secs = DateTime::parse_from_rfc3339("2026-06-14T00:00:00Z")
            .unwrap()
            .timestamp();
        assert_eq!(micros, secs * 1_000_000);
    }

    #[test]
    fn parse_at_invalid_is_usage_error() {
        assert_eq!(parse_at("not-a-time").unwrap_err().exit_code(), 2);
    }

    #[test]
    fn guard_force_allows_drop() {
        assert!(decide_guard(&["a.b".into()], true, false, |_| false).unwrap());
    }

    #[test]
    fn guard_non_tty_conflict_rejects() {
        assert_eq!(
            decide_guard(&["a.b".into()], false, false, |_| true)
                .unwrap_err()
                .exit_code(),
            1
        );
    }

    #[test]
    fn guard_tty_confirm_allows() {
        assert!(decide_guard(&["a.b".into()], false, true, |_| true).unwrap());
    }
}
